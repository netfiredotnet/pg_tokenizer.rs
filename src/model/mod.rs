mod builtin;
mod custom;
mod huggingface;
mod lindera;

use std::sync::{Arc, LazyLock};

use builtin::{get_builtin_model, is_builtin_model};
use custom::{CustomModel, CustomModelConfig};
use dashmap::{DashMap, Entry};
use huggingface::{HuggingFaceConfig, HuggingFaceModel};
use lindera::{LinderaConfig, LinderaModel};
use serde::{Deserialize, Serialize};

use crate::utils::spi_get_one;

pub trait TokenizerModel {
    fn apply(&self, token: String) -> Vec<u32>;

    fn apply_batch(&self, tokens: Vec<String>) -> Vec<u32> {
        tokens
            .into_iter()
            .flat_map(|token| self.apply(token))
            .collect()
    }
}
pub type TokenizerModelPtr = Arc<dyn TokenizerModel + Send + Sync>;

impl TokenizerModel for tokenizers::Tokenizer {
    fn apply(&self, token: String) -> Vec<u32> {
        self.encode_fast(token, false).unwrap().get_ids().to_vec()
    }
}

impl TokenizerModel for tocken::tokenizer::Tokenizer {
    fn apply(&self, token: String) -> Vec<u32> {
        self.tokenize(&token)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
enum ModelConfig {
    Custom(CustomModelConfig),
    Lindera(LinderaConfig),
    HuggingFace(HuggingFaceConfig),
}

type ModelObjectPool = DashMap<String, TokenizerModelPtr>;
pub(super) static MODEL_OBJECT_POOL: LazyLock<ModelObjectPool> =
    LazyLock::new(ModelObjectPool::default);

pgrx::extension_sql!(
    r#"
CREATE TABLE tokenizer_catalog.model (
    name TEXT NOT NULL UNIQUE PRIMARY KEY,
    config TEXT NOT NULL,
    owner NAME NOT NULL DEFAULT CURRENT_USER
);
"#,
    name = "model_table"
);

pgrx::extension_sql!(
    r#"
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_create_tokenizer(TEXT, TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_drop_tokenizer(TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_tokenize(TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_create_text_analyzer(TEXT, TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_drop_text_analyzer(TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_apply_text_analyzer(TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_create_stopwords(TEXT, TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_drop_stopwords(TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_create_synonym(TEXT, TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_drop_synonym(TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_create_huggingface_model(TEXT, TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_drop_huggingface_model(TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_create_lindera_model(TEXT, TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_drop_lindera_model(TEXT, TEXT) FROM PUBLIC;
REVOKE ALL ON FUNCTION tokenizer_catalog.__pg_tokenizer_apply_text_analyzer_for_custom_model(TEXT, TEXT) FROM PUBLIC;
"#,
    name = "revoke_internal_helper_execute_from_public",
    requires = [
        "create_tokenizer_wrapper_sql",
        "drop_tokenizer_wrapper_sql",
        "tokenize_wrapper_sql",
        "create_text_analyzer_wrapper_sql",
        "drop_text_analyzer_wrapper_sql",
        "apply_text_analyzer_wrapper_sql",
        "create_stopwords_wrapper_sql",
        "drop_stopwords_wrapper_sql",
        "create_synonym_wrapper_sql",
        "drop_synonym_wrapper_sql",
        "create_huggingface_model_wrapper_sql",
        "drop_huggingface_model_wrapper_sql",
        "create_lindera_model_wrapper_sql",
        "drop_lindera_model_wrapper_sql",
        "apply_text_analyzer_for_custom_model_wrapper_sql"
    ]
);

pub fn get_model(name: &str) -> TokenizerModelPtr {
    validate_model_name(name).unwrap();
    if let Some(model) = MODEL_OBJECT_POOL.get(name) {
        return model.clone();
    }

    match MODEL_OBJECT_POOL.entry(name.to_string()) {
        Entry::Occupied(entry) => entry.get().clone(),
        Entry::Vacant(entry) => {
            if let Some(object) = get_builtin_model(name) {
                entry.insert(object.clone());
                return object;
            }

            if let Some(object) = get_model_from_database(name) {
                entry.insert(object.clone());
                return object;
            }

            panic!("Model not found: {}", name);
        }
    }
}

fn get_model_from_database(name: &str) -> Option<TokenizerModelPtr> {
    let config = get_model_config(name)?;
    Some(build_model(name, &config))
}

fn get_model_config(name: &str) -> Option<ModelConfig> {
    let config_bytes: &str = spi_get_one(
        "SELECT config FROM tokenizer_catalog.model WHERE name = $1",
        &[name.into()],
    )?;

    serde_json::from_str(config_bytes).unwrap()
}

fn build_model(name: &str, config: &ModelConfig) -> TokenizerModelPtr {
    match config {
        ModelConfig::Custom(config) => Arc::new(CustomModel::new(name, config)),
        ModelConfig::Lindera(config) => Arc::new(LinderaModel::new(config)),
        ModelConfig::HuggingFace(config) => Arc::new(HuggingFaceModel::new(name, config)),
    }
}

// 1. It only contains ascii letters, numbers, and underscores.
// 2. It starts with a letter.
// 3. Its length cannot exceed 20 characters.
pub fn validate_model_name(name: &str) -> Result<(), String> {
    let name_bytes = name.as_bytes();
    for &b in name_bytes {
        if !b.is_ascii_alphanumeric() && b != b'_' {
            return Err(format!("Invalid character: {}", b as char));
        }
    }
    if !(1..=20).contains(&name_bytes.len()) {
        return Err(format!("Name length must be between 1 and {}", 20));
    }
    if !name_bytes[0].is_ascii_alphabetic() {
        return Err("Name must start with a letter".to_string());
    }

    Ok(())
}

pub fn validate_new_model_name(name: &str) -> Result<(), String> {
    validate_model_name(name)?;
    if is_builtin_model(name) {
        return Err("The name is reserved, please choose another name".to_string());
    }

    Ok(())
}

#[pgrx::pg_extern(volatile, parallel_unsafe)]
pub fn add_preload_model(name: &str) {
    validate_model_name(name).unwrap();
    get_model(name);

    let path = std::path::Path::new("pg_tokenizer/preload_models").join(name);
    if is_builtin_model(name) {
        std::fs::write(&path, "").unwrap();
    } else {
        let config = get_model_config(name).unwrap();
        std::fs::write(&path, serde_json::to_string(&config).unwrap()).unwrap();
    }
}

#[pgrx::pg_extern(volatile, parallel_unsafe)]
pub fn remove_preload_model(name: &str) {
    validate_model_name(name).unwrap();
    let path = std::path::Path::new("pg_tokenizer/preload_models").join(name);
    match std::fs::remove_file(&path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            pgrx::warning!("Preload model not found: {}", name);
        }
        Err(e) => {
            pgrx::error!("Failed to remove file: {}", e);
        }
    }
}

#[pgrx::pg_extern(volatile, parallel_unsafe)]
pub fn list_preload_models() -> Vec<String> {
    let dir_path = std::path::Path::new("pg_tokenizer/preload_models");
    let mut models = Vec::new();
    for entry in std::fs::read_dir(dir_path).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        models.push(name);
    }

    models
}

pub fn init() {
    let dir_path = std::path::Path::new("pg_tokenizer/preload_models");
    if !dir_path.exists() {
        match std::fs::create_dir_all(dir_path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                pgrx::error!("Failed to create directory: {}", e);
            }
        }
        for name in builtin::PRELOAD_MODELS {
            let path = dir_path.join(name);
            std::fs::write(&path, "").unwrap();
        }
    }

    for entry in std::fs::read_dir(dir_path).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        validate_model_name(&name).unwrap();

        if let Some(object) = get_builtin_model(&name) {
            MODEL_OBJECT_POOL.insert(name, object);
            continue;
        }

        let content = std::fs::read_to_string(entry.path()).unwrap();
        let config: ModelConfig = serde_json::from_str(&content).unwrap();
        let object = build_model(&name, &config);
        MODEL_OBJECT_POOL.insert(name, object);
    }
}

// unsafe fn get_model_from_database_without_spi(
//     name: &str,
// ) -> anyhow::Result<Option<TokenizerModelPtr>> {
//     let namespace_id = pgrx::pg_sys::get_namespace_oid(c"tokenizer_catalog".as_ptr(), false);
//     let table_oid = pgrx::pg_sys::get_relname_relid(c"model".as_ptr(), namespace_id);
//     anyhow::ensure!(table_oid != pgrx::pg_sys::InvalidOid, "Table not found");

//     let rel = pgrx::PgRelation::open(table_oid);
//     let scan = pgrx::pg_sys::table_beginscan(
//         rel.as_ptr(),
//         &raw mut pgrx::pg_sys::SnapshotAnyData,
//         0,
//         std::ptr::null_mut(),
//     );

//     let mut config: Option<ModelConfig> = None;

//     let mut tuple_ptr =
//         pgrx::pg_sys::heap_getnext(scan, pgrx::pg_sys::ScanDirection::ForwardScanDirection);
//     while tuple_ptr != std::ptr::null_mut() {
//         let tuple = PgHeapTuple::from_heap_tuple(rel.tuple_desc(), tuple_ptr);
//         let tuple_name: &str = tuple
//             .get_by_index(NonZeroUsize::new(1).unwrap())?
//             .ok_or(anyhow::anyhow!("Name is null"))?;
//         if tuple_name != name {
//             tuple_ptr =
//                 pgrx::pg_sys::heap_getnext(scan, pgrx::pg_sys::ScanDirection::ForwardScanDirection);
//             continue;
//         }

//         let config_str: &str = tuple
//             .get_by_index(NonZeroUsize::new(2).unwrap())?
//             .ok_or(anyhow::anyhow!("Config is null"))?;
//         config = Some(serde_json::from_str(config_str)?);
//         break;
//     }
//     pgrx::pg_sys::table_endscan(scan);

//     let Some(config) = config else {
//         return Ok(None);
//     };
//     Ok(Some(build_model(name, &config)))
// }
