use crate::language_registry::LanguageRegistry;
use ignore::{WalkBuilder, WalkState};
use rusqlite::{self, Connection, Transaction};
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use tree_sitter::{Parser, Point, PropertyMatcher, PropertySheet, Tree};

#[derive(Debug)]
pub enum Error {
    IO(io::Error),
    Ignore(ignore::Error),
    SQL(rusqlite::Error),
}

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Clone)]
pub struct Index {
    db_path: PathBuf,
    language_registry: Arc<Mutex<LanguageRegistry>>,
}

struct Scope<'a> {
    kind: Option<&'a str>,
    local_refs: Vec<(&'a str, Point)>,
    local_defs: Vec<(&'a str, Point)>,
    hoisted_local_defs: HashMap<&'a str, Point>,
}

struct Walker<'a> {
    scope_stack: Vec<Scope<'a>>,
    db: Transaction<'a>,
    property_matcher: PropertyMatcher<'a>,
    source_code: &'a str,
    file_id: i64,
}

impl<'a> Walker<'a> {
    fn new(
        db: Transaction<'a>,
        file_id: i64,
        tree: &'a Tree,
        property_sheet: &'a PropertySheet,
        source_code: &'a str,
    ) -> Self {
        Self {
            db,
            source_code,
            property_matcher: PropertyMatcher::new(tree, property_sheet),
            scope_stack: Vec::new(),
            file_id,
        }
    }

    fn index_tree(&mut self) -> Result<()> {
        self.push_scope(None);
        let mut visited_node = false;
        loop {
            if visited_node {
                if self.property_matcher.goto_next_sibling() {
                    self.enter_node();
                    visited_node = false;
                } else if self.property_matcher.goto_parent() {
                    self.leave_node()?;
                } else {
                    break;
                }
            } else if self.property_matcher.goto_first_child() {
                self.enter_node();
            } else {
                visited_node = true;
            }
        }
        self.pop_scope()?;
        Ok(())
    }

    fn enter_node(&mut self) {
        let node = self.property_matcher.node();
        let props = self.property_matcher.node_properties();
        let scope_type = props.get("scope-type").map(|s| s.as_str());
        let mut is_definition = false;

        match props.get("define").map(|s| s.as_ref()) {
            Some("local") => {
                is_definition = true;
                if let Some(text) = node.utf8_text(self.source_code).ok() {
                    let scope = self.top_scope(scope_type);
                    let local_def = (text, node.start_position());
                    if props.get("hoisted").is_some() {
                        scope.local_defs.push(local_def);
                    } else {
                        scope.hoisted_local_defs.insert(local_def.0, local_def.1);
                    }
                }
            }
            Some("scope") => self.push_scope(scope_type),
            _ => {}
        };

        match props.get("reference").map(|s| s.as_ref()) {
            Some("local") => {
                if !is_definition {
                    if let Some(text) = node.utf8_text(self.source_code).ok() {
                        self.top_scope(scope_type)
                            .local_refs
                            .push((text, node.start_position()));
                    }
                }
            }
            _ => {}
        }
    }

    fn leave_node(&mut self) -> Result<()> {
        let props = self.property_matcher.node_properties();
        match props.get("define").map(|s| s.as_ref()) {
            Some("scope") => {
                self.pop_scope()?;
            }
            _ => {}
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
            })
            .unwrap()
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
            local_def_ids.push(self.insert_local_def(name, *position)?);
        }

        let mut hoisted_local_def_ids = HashMap::new();
        for (name, position) in scope.hoisted_local_defs.iter() {
            hoisted_local_def_ids.insert(
                name,
                self.insert_local_def(name, *position)?,
            );
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
                self.insert_local_ref(local_def_id, local_ref.0, local_ref.1)?;
            } else if let Some(parent_scope) = parent_scope.as_mut() {
                parent_scope.local_refs.push(local_ref);
            }
        }
        parent_scope.map(|scope| self.scope_stack.push(scope));

        Ok(())
    }

    fn insert_local_ref(&mut self, local_def_id: i64, name: &'a str, position: Point) -> Result<()> {
        self.db.execute(
            "
                INSERT INTO local_references
                (file_id, definition_id, row, column, length)
                VALUES
                (?1, ?2, ?3, ?4, ?5)
            ",
            &[
                &self.file_id,
                &local_def_id,
                &position.row,
                &position.column,
                &(name.as_bytes().len() as i64),
            ],
        )?;
        Ok(())
    }

    fn insert_local_def(&mut self, name: &'a str, position: Point) -> Result<i64> {
        self.db.execute(
            "
                INSERT INTO local_definitions
                (file_id, row, column, length)
                VALUES
                (?1, ?2, ?3, ?4)
            ",
            &[
                &self.file_id,
                &position.row,
                &position.column,
                &(name.as_bytes().len() as i64),
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }
}

impl Index {
    pub fn new(config_dir: PathBuf) -> Result<Self> {
        Ok(Index {
            db_path: config_dir.join("db.sqlite"),
            language_registry: Arc::new(Mutex::new(LanguageRegistry::new(
                config_dir,
                vec![

                    "/Users/max/github".into()
                ],
            )?)),
        })
    }

    pub fn index_path(&mut self, path: PathBuf) -> Result<()> {
        let last_error = Arc::new(Mutex::new(Ok(())));
        let db = Connection::open(&self.db_path)?;
        db.execute_batch(include_str!("./schema.sql")).expect("Failed to ensure schema is set up");

        WalkBuilder::new(path).threads(1).build_parallel().run(|| {
            let worker = self.clone();
            let last_error = last_error.clone();
            let mut parser = Parser::new();
            match Connection::open(&self.db_path) {
                Ok(mut db) => Box::new({
                    move |entry| {
                        match entry {
                            Ok(entry) => {
                                if let Some(t) = entry.file_type() {
                                    if t.is_file() {
                                        if let Err(e) =
                                            worker.index_file(&mut db, &mut parser, entry.path())
                                        {
                                            *last_error.lock().unwrap() = Err(e);
                                            return WalkState::Quit;
                                        }
                                    }
                                }
                            },
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

    pub fn find_definition(&mut self, path: PathBuf, position: Point) -> Result<Vec<(PathBuf, Point, usize)>> {
        let db = Connection::open(&self.db_path)?;
        let file_id: i64 = db.query_row(
            "SELECT id FROM files WHERE path = ?1",
            &[&path.as_os_str().as_bytes()],
            |row| row.get(0)
        )?;
        let (position, length) = db.query_row(
            "
                SELECT
                    local_definitions.row,
                    local_definitions.column,
                    local_definitions.length
                FROM
                    local_references,
                    local_definitions
                WHERE
                    local_references.definition_id = local_definitions.id AND
                    local_references.file_id = ?1 AND
                    local_references.row = ?2 AND
                    local_references.column <= ?3 AND
                    local_references.column + local_references.length > ?3
            ",
            &[
                &file_id,
                &(position.row as i64),
                &(position.column as i64),
            ],
            |row| (Point { row: row.get(0), column: row.get(1) }, row.get::<usize, i64>(2))
        )?;
        Ok(vec![(path, position, length as usize)])
    }

    fn index_file(&self, db: &mut Connection, parser: &mut Parser, path: &Path) -> Result<()> {
        let mut file = File::open(path)?;
        if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
            if let Some((language, property_sheet)) = self
                .language_registry
                .lock()
                .unwrap()
                .language_for_file_extension(extension)?
            {
                parser
                    .set_language(language)
                    .expect("Incompatible language version");
                let mut source_code = String::new();
                file.read_to_string(&mut source_code)?;
                let tree = parser
                    .parse_str(&source_code, None)
                    .expect("Parsing failed");
                let tx = db.transaction()?;
                tx.execute(
                    "DELETE FROM files WHERE path = ?1",
                    &[&path.as_os_str().as_bytes()],
                )?;
                tx.execute(
                    "INSERT INTO files (path) VALUES (?1)",
                    &[&path.as_os_str().as_bytes()],
                )?;
                let file_id = tx.last_insert_rowid();
                let mut walker = Walker::new(tx, file_id, &tree, &property_sheet, &source_code);
                walker.index_tree()?;
                walker.db.commit()?;
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
            Error::Ignore(e) => e.fmt(f)
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
