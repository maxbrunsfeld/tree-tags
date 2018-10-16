use crate::language_registry::LanguageRegistry;
use crate::store::{Store, StoreFile};
use ignore::{WalkBuilder, WalkState};
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tree_sitter::{Parser, Point, PropertySheet, Tree, TreePropertyCursor};

pub struct DirCrawler {
    store: Store,
    language_registry: Arc<Mutex<LanguageRegistry>>,
    parser: Parser,
}

struct TreeCrawler<'a> {
    store: StoreFile<'a>,
    scope_stack: Vec<Scope<'a>>,
    module_stack: Vec<Module<'a>>,
    property_matcher: TreePropertyCursor<'a>,
    source_code: &'a str,
}

struct Definition<'a> {
    name: Option<(&'a str, Point)>,
    kind: Option<&'a str>,
    start_position: Point,
    end_position: Point,
}

struct Module<'a> {
    name: Option<&'a str>,
    definitions: Vec<Definition<'a>>,
    pending_definition_stack: Vec<Definition<'a>>,
}

struct Scope<'a> {
    kind: Option<&'a str>,
    local_refs: Vec<(&'a str, Point)>,
    local_defs: Vec<(&'a str, Point)>,
    hoisted_local_defs: HashMap<&'a str, Point>,
}

#[derive(Debug)]
pub enum Error {
    IO(io::Error),
    Ignore(ignore::Error),
    SQL(rusqlite::Error),
}

pub type Result<T> = core::result::Result<T, Error>;

impl<'a> TreeCrawler<'a> {
    fn new(
        store: StoreFile<'a>,
        tree: &'a Tree,
        property_sheet: &'a PropertySheet,
        source_code: &'a str,
    ) -> Self {
        Self {
            store,
            source_code,
            property_matcher: tree.walk_with_properties(property_sheet),
            scope_stack: Vec::new(),
            module_stack: Vec::new(),
        }
    }

    fn crawl_tree(&mut self) -> Result<()> {
        self.push_scope(None);
        self.push_module();
        let mut visited_node = false;
        loop {
            if visited_node {
                if self.property_matcher.goto_next_sibling() {
                    self.enter_node()?;
                    visited_node = false;
                } else if self.property_matcher.goto_parent() {
                    self.leave_node()?;
                } else {
                    break;
                }
            } else if self.property_matcher.goto_first_child() {
                self.enter_node()?;
            } else {
                visited_node = true;
            }
        }
        self.pop_module()?;
        self.pop_scope()?;
        Ok(())
    }

    fn enter_node(&mut self) -> Result<()> {
        let node = self.property_matcher.node();
        let mut is_local_def = false;

        if self.has_property_value("local-definition", "true") {
            is_local_def = true;
            let scope_type = self.get_property("scope-type");
            let is_hoisted = self.has_property("local-is-hoisted");
            if let Some(text) = node.utf8_text(self.source_code).ok() {
                if is_hoisted {
                    self.top_scope(scope_type)
                        .hoisted_local_defs
                        .insert(text, node.start_position());
                } else {
                    self.top_scope(scope_type)
                        .local_defs
                        .push((text, node.start_position()));
                }
            }
        }

        if self.has_property_value("local-reference", "true") && !is_local_def {
            if let Some(text) = node.utf8_text(self.source_code).ok() {
                self.top_scope(self.get_property("scope-type"))
                    .local_refs
                    .push((text, node.start_position()));
            }
        }

        if self.has_property_value("local-scope", "true") {
            self.push_scope(self.get_property("scope-type"));
        }

        if self.has_property_value("module", "true") {
            self.push_module();
        }

        match self.get_property("module-part") {
            Some("name") => {
                if let Some(text) = node.utf8_text(self.source_code).ok() {
                    let module = self.module_stack.last_mut().unwrap();
                    module.name = Some(text);
                }
            }
            _ => {}
        }

        if self.has_property_value("definition", "true") {
            let kind = self.get_property("definition-type");
            self.top_module().pending_definition_stack.push(Definition {
                name: None,
                kind,
                start_position: node.start_position(),
                end_position: node.end_position(),
            });
        }

        match self.get_property("definition-part") {
            Some("name") => {
                if let Some(text) = node.utf8_text(self.source_code).ok() {
                    self.top_definition().unwrap().name = Some((text, node.start_position()));
                }
            }
            Some("value") => {
                let kind = self.get_property("definition-type");
                if kind.is_some() {
                    self.top_definition().unwrap().kind = kind;
                }
            }
            _ => {}
        }

        if self.has_property_value("reference", "true") {
            if let Some(text) = node.utf8_text(self.source_code).ok() {
                self.store.insert_ref(
                    text,
                    node.start_position(),
                    self.get_property("reference-type"),
                )?;
            }
        }

        Ok(())
    }

    fn leave_node(&mut self) -> Result<()> {
        if self.has_property("local-scope") {
            self.pop_scope()?;
        }

        if self.has_property("definition") {
            self.pop_definition()?;
        }

        if self.has_property("module") {
            self.pop_module()?;
        }

        Ok(())
    }

    fn top_scope(&mut self, kind: Option<&'a str>) -> &mut Scope<'a> {
        self.scope_stack
            .iter_mut()
            .enumerate()
            .rev()
            .find_map(|(i, scope)| {
                if i == 0 || kind.map_or(true, |kind| Some(kind) == scope.kind) {
                    Some(scope)
                } else {
                    None
                }
            }).unwrap()
    }

    fn top_module(&mut self) -> &mut Module<'a> {
        self.module_stack.last_mut().unwrap()
    }

    fn top_definition(&mut self) -> Option<&mut Definition<'a>> {
        self.top_module().pending_definition_stack.last_mut()
    }

    fn push_scope(&mut self, kind: Option<&'a str>) {
        self.scope_stack.push(Scope {
            kind,
            local_refs: Vec::new(),
            local_defs: Vec::new(),
            hoisted_local_defs: HashMap::new(),
        });
    }

    fn pop_scope(&mut self) -> Result<()> {
        let mut scope = self.scope_stack.pop().unwrap();

        let mut local_def_ids = Vec::with_capacity(scope.local_defs.len());
        for (name, position) in scope.local_defs.iter() {
            local_def_ids.push(self.store.insert_local_def(name, *position)?);
        }

        let mut hoisted_local_def_ids = HashMap::new();
        for (name, position) in scope.hoisted_local_defs.iter() {
            hoisted_local_def_ids.insert(name, self.store.insert_local_def(name, *position)?);
        }

        let mut parent_scope = self.scope_stack.pop();
        for local_ref in scope.local_refs.drain(..) {
            let mut local_def_id = None;

            for (i, local_def) in scope.local_defs.iter().enumerate() {
                if local_def.1 > local_ref.1 {
                    break;
                }
                if local_def.0 == local_ref.0 {
                    local_def_id = Some(local_def_ids[i]);
                }
            }

            if local_def_id.is_none() {
                local_def_id = hoisted_local_def_ids.get(&local_ref.0).cloned();
            }

            if let Some(local_def_id) = local_def_id {
                self.store
                    .insert_local_ref(local_def_id, local_ref.0, local_ref.1)?;
            } else if let Some(parent_scope) = parent_scope.as_mut() {
                parent_scope.local_refs.push(local_ref);
            }
        }
        parent_scope.map(|scope| self.scope_stack.push(scope));

        Ok(())
    }

    fn push_module(&mut self) {
        self.module_stack.push(Module {
            name: None,
            definitions: Vec::new(),
            pending_definition_stack: Vec::new(),
        });
    }

    fn pop_module(&mut self) -> Result<()> {
        let mod_path = self
            .module_stack
            .iter()
            .filter_map(|m| m.name)
            .collect::<Vec<_>>();
        let module = self.module_stack.pop().unwrap();
        for definition in module.definitions {
            if let Some((name, name_position)) = definition.name {
                self.store.insert_def(
                    name,
                    name_position,
                    definition.start_position,
                    definition.end_position,
                    definition.kind,
                    &mod_path,
                )?;
            }
        }
        Ok(())
    }

    fn pop_definition(&mut self) -> Result<()> {
        let module = self.module_stack.last_mut().unwrap();
        let definition = module.pending_definition_stack.pop().unwrap();
        module.definitions.push(definition);
        Ok(())
    }

    fn get_property(&self, prop: &'static str) -> Option<&'a str> {
        self.property_matcher
            .node_properties()
            .get(prop)
            .map(|v| v.as_str())
    }

    fn has_property(&self, prop: &'static str) -> bool {
        self.get_property(prop).is_some()
    }

    fn has_property_value(&self, prop: &'static str, value: &'static str) -> bool {
        self.get_property(prop) == Some(value)
    }
}

impl DirCrawler {
    pub fn new(store: Store, language_registry: LanguageRegistry) -> Self {
        Self {
            store: store,
            language_registry: Arc::new(Mutex::new(language_registry)),
            parser: Parser::new(),
        }
    }

    fn clone(&self) -> Result<Self> {
        Ok(Self {
            store: self.store.clone()?,
            language_registry: self.language_registry.clone(),
            parser: Parser::new(),
        })
    }

    pub fn crawl_path(&mut self, path: PathBuf) -> Result<()> {
        let last_error = Arc::new(Mutex::new(Ok(())));

        self.store
            .initialize()
            .expect("Failed to ensure schema is set up");

        WalkBuilder::new(path).build_parallel().run(|| {
            let last_error = last_error.clone();
            match self.clone() {
                Ok(mut crawler) => Box::new({
                    move |entry| {
                        match entry {
                            Ok(entry) => {
                                if let Some(t) = entry.file_type() {
                                    if t.is_file() {
                                        if let Err(e) = crawler.crawl_file(entry.path()) {
                                            *last_error.lock().unwrap() = Err(e);
                                            return WalkState::Quit;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                *last_error.lock().unwrap() = Err(e.into());
                            }
                        }
                        WalkState::Continue
                    }
                }),
                Err(error) => {
                    *last_error.lock().unwrap() = Err(error.into());
                    Box::new(|_| WalkState::Quit)
                }
            }
        });

        Arc::try_unwrap(last_error).unwrap().into_inner().unwrap()
    }

    fn crawl_file(&mut self, path: &Path) -> Result<()> {
        let mut file = File::open(path)?;
        if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
            if let Some((language, property_sheet)) = self
                .language_registry
                .lock()
                .unwrap()
                .language_for_file_extension(extension)?
            {
                self.parser
                    .set_language(language)
                    .expect("Incompatible language version");
                let mut source_code = String::new();
                file.read_to_string(&mut source_code)?;
                let tree = self
                    .parser
                    .parse_str(&source_code, None)
                    .expect("Parsing failed");
                let store = self.store.file(path)?;
                let mut crawler = TreeCrawler::new(store, &tree, &property_sheet, &source_code);
                crawler.crawl_tree()?;
                crawler.store.commit()?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::IO(e) => e.fmt(f),
            Error::SQL(e) => e.fmt(f),
            Error::Ignore(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::IO(e)
    }
}

impl From<ignore::Error> for Error {
    fn from(e: ignore::Error) -> Error {
        Error::Ignore(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Error {
        Error::SQL(e)
    }
}
