use libloading::{Library, Symbol};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tree_sitter::{Language, PropertySheet};

const PACKAGE_JSON_PATH: &'static str = "package.json";
const PARSER_C_PATH: &'static str = "src/parser.c";
const SCANNER_C_PATH: &'static str = "src/scanner.c";
const SCANNER_CC_PATH: &'static str = "src/scanner.cc";
const DEFINITIONS_JSON_PATH: &'static str = "src/definitions.json";

#[cfg(unix)]
const DYLIB_EXTENSION: &'static str = "so";

#[cfg(windows)]
const DYLIB_EXTENSION: &'static str = "dll";

pub struct LanguageRegistry {
    config_path: PathBuf,
    language_names_by_extension: HashMap<String, (String, PathBuf)>,
    loaded_languages: HashMap<String, (Library, Language, Arc<PropertySheet>)>,
}

unsafe impl Send for LanguageRegistry {}
unsafe impl Sync for LanguageRegistry {}

impl LanguageRegistry {
    pub fn new(config_path: PathBuf, parser_dirs: Vec<PathBuf>) -> io::Result<Self> {
        let mut language_names_by_extension = HashMap::new();
        for parser_container_dir in parser_dirs.iter() {
            for entry in fs::read_dir(parser_container_dir)? {
                let entry = entry?;
                if let Some(parser_dir_name) = entry.file_name().to_str() {
                    if parser_dir_name.starts_with("tree-sitter-") {
                        let name = parser_dir_name.split_at("tree-sitter-".len()).1;
                        let language_path = entry.path();
                        match file_extensions_for_language_path(&language_path) {
                            Ok(None) => {},
                            Ok(Some(extensions)) => {
                                for extension in extensions {
                                    language_names_by_extension.insert(
                                        extension.to_owned(),
                                        (name.to_owned(), entry.path())
                                    );
                                }
                            },
                            Err(e) => {
                                eprintln!("{}: {}", parser_dir_name, e);
                            }
                        }
                    }
                }
            }
        }

        Ok(LanguageRegistry {
            config_path,
            loaded_languages: HashMap::new(),
            language_names_by_extension,
        })
    }

    pub fn language_for_file_extension(&mut self, extension: &str) -> io::Result<Option<(Language, Arc<PropertySheet>)>> {
        if let Some((name, path)) = self.language_names_by_extension.get(extension) {
            if let Some((_, language, sheet)) = self.loaded_languages.get(name) {
                return Ok(Some((*language, sheet.clone())));
            }
            self.load_language_at_path(&name.clone(), &path.clone())
        } else {
            Ok(None)
        }
    }

    fn load_language_at_path(
        &mut self,
        name: &str,
        language_path: &Path,
    ) -> io::Result<Option<(Language, Arc<PropertySheet>)>> {
        let parser_c_path = language_path.join(PARSER_C_PATH);
        let mut library_path = self.config_path.join("lib").join(name);
        library_path.set_extension(DYLIB_EXTENSION);

        if !library_path.exists() || was_modified_more_recently(&parser_c_path, &library_path)? {
            let compiler_name = std::env::var("CXX").unwrap_or("c++".to_owned());
            let mut command = Command::new(compiler_name);
            command
                .arg("-shared")
                .arg("-fPIC")
                .arg("-I")
                .arg(language_path.join("src"))
                .arg("-o")
                .arg(&library_path)
                .arg("-xc")
                .arg(parser_c_path);
            let scanner_c_path = language_path.join(SCANNER_C_PATH);
            let scanner_cc_path = language_path.join(SCANNER_CC_PATH);
            if scanner_c_path.exists() {
                command.arg("-xc").arg(scanner_c_path);
            } else if scanner_cc_path.exists() {
                command.arg("-xc++").arg(scanner_cc_path);
            }
            command.output()?;
        }

        let library = Library::new(library_path)?;
        let language_fn_name = "tree_sitter_".to_owned() + name;
        let language = unsafe {
            let language_fn: Symbol<unsafe extern "C" fn() -> Language> =
                library.get(language_fn_name.as_bytes())?;
            language_fn()
        };

        let mut property_sheet_string = String::new();
        let mut property_sheet_file = File::open(language_path.join(DEFINITIONS_JSON_PATH))?;
        property_sheet_file.read_to_string(&mut property_sheet_string)?;
        let property_sheet = Arc::new(PropertySheet::new(language, &property_sheet_string)?);
        self.loaded_languages.insert(name.to_string(), (library, language, property_sheet.clone()));
        Ok(Some((language, property_sheet)))
    }
}

fn file_extensions_for_language_path(path: &Path) -> io::Result<Option<Vec<String>>> {
    #[derive(Deserialize)]
    struct TreeSitterJSON {
        #[serde(rename = "file-types")]
        file_types: Option<Vec<String>>
    }

    #[derive(Deserialize)]
    struct PackageJSON {
        #[serde(rename = "tree-sitter")]
        tree_sitter: Option<TreeSitterJSON>
    }

    let mut package_json_contents = String::new();
    let mut package_json_file = File::open(path.join(PACKAGE_JSON_PATH))?;
    package_json_file.read_to_string(&mut package_json_contents)?;
    let package_json: PackageJSON = serde_json::from_str(&package_json_contents)?;
    Ok(package_json.tree_sitter.and_then(|t| t.file_types))
}

fn was_modified_more_recently(a: &Path, b: &Path) -> io::Result<bool> {
    Ok(fs::metadata(a)?.modified()? > fs::metadata(b)?.modified()?)
}
