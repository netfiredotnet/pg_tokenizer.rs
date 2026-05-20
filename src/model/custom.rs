use std::{
    collections::HashMap,
    ffi::{c_void, CStr},
    fmt::Write,
    sync::{Arc, LazyLock},
};

use pgrx::{
    pg_sys::{panic::ErrorReportable, AsPgCStr},
    prelude::PgHeapTuple,
    WhoAllocated,
};
use serde::{Deserialize, Serialize};
use validator::{Validate, ValidationError};

use crate::{text_analyzer::get_text_analyzer, utils::spi_get_one};

use super::{validate_new_model_name, ModelConfig, TokenizerModel, MODEL_OBJECT_POOL};

static INTERNAL_CUSTOM_MODEL_CALL_TOKEN: LazyLock<String> = LazyLock::new(|| {
    let mut bytes = [0u8; 32];
    let ok = unsafe {
        pgrx::pg_sys::pg_strong_random(bytes.as_mut_ptr().cast::<c_void>(), bytes.len())
    };
    if !ok {
        panic!("could not initialize pg_tokenizer internal call token");
    }

    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut token, "{byte:02x}").unwrap();
    }
    token
});

fn internal_custom_model_call_token() -> &'static str {
    &INTERNAL_CUSTOM_MODEL_CALL_TOKEN
}

fn ensure_internal_custom_model_call_token(token: &str) {
    if token != internal_custom_model_call_token() {
        panic!("Permission denied: invalid internal custom-model call token");
    }
}

fn effective_caller() -> String {
    spi_get_one(
        "SELECT CASE WHEN pg_catalog.current_setting('role') = 'none' THEN session_user::text ELSE pg_catalog.current_setting('role') END",
        &[],
    )
    .unwrap()
}

fn ensure_effective_caller(owner: &str) {
    if owner != effective_caller() {
        panic!("Permission denied: owner does not match effective caller");
    }
}

#[derive(Debug)]
pub struct CustomModel {
    name: String,
}

impl CustomModel {
    pub fn new(name: &str, _config: &CustomModelConfig) -> Self {
        CustomModel {
            name: name.to_string(),
        }
    }
}

impl TokenizerModel for CustomModel {
    fn apply(&self, text: String) -> Vec<u32> {
        let query = format!(
            r#"SELECT id FROM tokenizer_catalog."model_{}" WHERE token = $1"#,
            self.name
        );

        let id = spi_get_one::<i32>(&query, &[text.into()]);

        if let Some(id) = id {
            vec![u32::try_from(id).unwrap()]
        } else {
            vec![]
        }
    }

    fn apply_batch(&self, tokens: Vec<String>) -> Vec<u32> {
        let query = format!(
            r#"SELECT id, token FROM tokenizer_catalog."model_{}" WHERE token = ANY($1)"#,
            self.name
        );

        let mut token_map = HashMap::new();
        pgrx::Spi::connect(|client| {
            let tuptable = client
                .select(&query, None, &[tokens.clone().into()])
                .unwrap_or_report();
            for tup in tuptable {
                let id: i32 = tup.get(1).unwrap_or_report().expect("no id value");
                let id = u32::try_from(id).expect("id is not a valid u32");
                let token: String = tup.get(2).unwrap_or_report().expect("no token value");
                token_map.insert(token, id);
            }
        });

        tokens
            .into_iter()
            .filter_map(|token| token_map.get(&token).copied())
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
#[serde(deny_unknown_fields)]
#[validate(schema(function = "CustomModelConfig::validate_column_name"))]
pub struct CustomModelConfig {
    table: String,
    column: String,
    text_analyzer: String,
    #[serde(default)]
    resolved_table: Option<String>,
}

impl CustomModelConfig {
    fn validate_column_name(&self) -> Result<(), ValidationError> {
        if self.column.contains("$col$") {
            return Err(ValidationError::new("column name cannot contain '$col$'"));
        }
        Ok(())
    }
}

#[pgrx::pg_extern(volatile, parallel_safe)]
pub fn create_custom_model(name: &str, config: &str) {
    validate_new_model_name(name).unwrap();

    let mut config: CustomModelConfig = toml::from_str(config).unwrap();
    let caller_schema: &str = spi_get_one("SELECT pg_catalog.current_schema()::text", &[]).unwrap();
    let table = resolve_qualified_table_name(&config.table, caller_schema);
    let column = quote_identifier(&config.column);
    let model_name = quote_literal(name);
    let target_column = quote_literal(&config.column);
    let text_analyzer = quote_literal(&config.text_analyzer);
    config.resolved_table = Some(table.clone());
    let owner: &str = spi_get_one("SELECT current_user::text", &[]).unwrap();

    ensure_custom_model_privileges(owner, &table, &config.column);

    let collect_tokens = format!(
        r#"
        SELECT DISTINCT token FROM (
            SELECT unnest(tokenizer_catalog.apply_text_analyzer_for_custom_model({}, {})) AS token
            FROM {}
        ) tokens
        "#,
        column, text_analyzer, table
    );
    let create_trigger = format!(
        r#"
        CREATE TRIGGER "model_{}_trigger"
        BEFORE INSERT OR UPDATE OF {}
        ON {}
        FOR EACH ROW
        EXECUTE FUNCTION tokenizer_catalog.custom_model_insert_trigger({}, {}, {});
        "#,
        name, column, table, model_name, target_column, text_analyzer
    );
    let config_str = serde_json::to_string(&ModelConfig::Custom(config)).unwrap();

    pgrx::Spi::connect_mut(|client| {
        let mut tokens = Vec::new();
        let tuptable = client.select(&collect_tokens, None, &[]).unwrap_or_report();
        for tup in tuptable {
            let token: String = tup.get(1).unwrap_or_report().expect("no token value");
            tokens.push(token);
        }

        client.update(&create_trigger, None, &[]).unwrap();
        client
            .update(
                "SELECT tokenizer_catalog.__pg_tokenizer_create_custom_model_catalog($1, $2, $3, $4, $5)",
                None,
                &[
                    internal_custom_model_call_token().into(),
                    name.into(),
                    config_str.into(),
                    owner.into(),
                    tokens.into(),
                ],
            )
            .unwrap_or_report();
    });
}

#[pgrx::pg_extern(volatile, parallel_safe)]
pub fn drop_custom_model(name: &str) {
    if let Err(e) = validate_new_model_name(name) {
        pgrx::warning!("Invalid model name: {}, Details: {}", name, e);
        return;
    }

    let owner: &str = spi_get_one("SELECT current_user::text", &[]).unwrap();
    let config_bytes: &str = spi_get_one(
        "SELECT tokenizer_catalog.__pg_tokenizer_get_custom_model_config($1, $2, $3)",
        &[
            internal_custom_model_call_token().into(),
            name.into(),
            owner.into(),
        ],
    )
    .unwrap();
    let config: ModelConfig = serde_json::from_str(config_bytes).unwrap();
    let ModelConfig::Custom(config) = &config else {
        panic!("Model is not a custom model: {}", name);
    };
    let caller_schema: &str = spi_get_one("SELECT pg_catalog.current_schema()::text", &[]).unwrap();
    let table_name = config
        .resolved_table
        .clone()
        .unwrap_or_else(|| resolve_qualified_table_name(&config.table, caller_schema));

    let drop_model_trigger = format!(
        r#"DROP TRIGGER IF EXISTS "model_{}_trigger" ON {}"#,
        name, table_name
    );
    let drop_insert_trigger = format!(
        r#"DROP TRIGGER IF EXISTS "model_{}_trigger_insert" ON {}"#,
        name, table_name
    );
    pgrx::Spi::connect_mut(|client| {
        client.update(&drop_model_trigger, None, &[]).unwrap();
        client.update(&drop_insert_trigger, None, &[]).unwrap();
        client
            .update(
                "SELECT tokenizer_catalog.__pg_tokenizer_drop_custom_model_catalog($1, $2, $3)",
                None,
                &[
                    internal_custom_model_call_token().into(),
                    name.into(),
                    owner.into(),
                ],
            )
            .unwrap_or_report();
    });
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_create_custom_model_catalog",
    volatile,
    parallel_safe,
    security_definer
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn create_custom_model_catalog_internal(
    call_token: &str,
    name: &str,
    config: &str,
    owner: &str,
    tokens: Vec<String>,
) {
    ensure_internal_custom_model_call_token(call_token);
    ensure_effective_caller(owner);
    validate_new_model_name(name).unwrap();

    let create_word_table = format!(
        r#"
        CREATE TABLE tokenizer_catalog."model_{}" (
            id int GENERATED BY DEFAULT AS IDENTITY,
            token TEXT NOT NULL UNIQUE
        )
        "#,
        name
    );
    let insert_tokens = format!(
        r#"
        INSERT INTO tokenizer_catalog."model_{}" (token)
        SELECT DISTINCT unnest($1::text[])
        ON CONFLICT (token) DO NOTHING
        "#,
        name
    );
    let insert_model = r#"
        INSERT INTO tokenizer_catalog.model (name, config, owner) VALUES ($1, $2, $3)
        ON CONFLICT (name) DO NOTHING RETURNING 1
        "#;

    pgrx::Spi::connect_mut(|client| {
        client.update(&create_word_table, None, &[]).unwrap();
        client
            .update(&insert_tokens, None, &[tokens.into()])
            .unwrap_or_report();
        let tuptable = client
            .update(insert_model, None, &[name.into(), config.into(), owner.into()])
            .unwrap();
        if tuptable.is_empty() {
            panic!("Model already exists: {}", name);
        }

        let config: ModelConfig = serde_json::from_str(config).unwrap();
        let ModelConfig::Custom(config) = &config else {
            panic!("Model is not a custom model: {}", name);
        };
        MODEL_OBJECT_POOL.insert(name.to_string(), Arc::new(CustomModel::new(name, config)));
    });
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_get_custom_model_config",
    volatile,
    parallel_safe,
    security_definer
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn get_custom_model_config_internal(call_token: &str, name: &str, owner: &str) -> String {
    ensure_internal_custom_model_call_token(call_token);
    ensure_effective_caller(owner);
    validate_new_model_name(name).unwrap();

    let select_config = r#"SELECT config FROM tokenizer_catalog.model WHERE name = $1 AND owner = $2"#;
    let config_bytes: &str = spi_get_one(select_config, &[name.into(), owner.into()]).unwrap();
    let config: ModelConfig = serde_json::from_str(config_bytes).unwrap();
    let ModelConfig::Custom(_) = &config else {
        panic!("Model is not a custom model: {}", name);
    };
    config_bytes.to_string()
}

#[pgrx::pg_extern(
    name = "__pg_tokenizer_drop_custom_model_catalog",
    volatile,
    parallel_safe,
    security_definer
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn drop_custom_model_catalog_internal(call_token: &str, name: &str, owner: &str) {
    ensure_internal_custom_model_call_token(call_token);
    ensure_effective_caller(owner);
    validate_new_model_name(name).unwrap();

    let drop_table = format!(r#"DROP TABLE IF EXISTS tokenizer_catalog."model_{}""#, name);
    let delete_model = r#"DELETE FROM tokenizer_catalog.model WHERE name = $1 AND owner = $2 RETURNING 1"#;

    pgrx::Spi::connect_mut(|client| {
        let tuptable = client
            .update(delete_model, None, &[name.into(), owner.into()])
            .unwrap();
        if tuptable.is_empty() {
            panic!("Model not found or not owned by current user: {}", name);
        }
        client.update(&drop_table, None, &[]).unwrap();
        MODEL_OBJECT_POOL.remove(name);
    });
}

const MAX_TOKEN_LENGTH: usize = 2600;

#[pgrx::pg_extern(
    name = "__pg_tokenizer_apply_text_analyzer_for_custom_model",
    volatile,
    parallel_safe,
    security_definer,
)]
#[pgrx::search_path(tokenizer_catalog, pg_catalog)]
fn apply_text_analyzer_for_custom_model_internal(text: &str, text_analyzer_name: &str) -> Vec<String> {
    let text_analyzer = get_text_analyzer(text_analyzer_name);
    let mut results = text_analyzer.apply(text);

    // split all tokens that are longer than 2600 characters
    let len = results.len();
    for i in 0..len {
        let token_len = results[i].len();
        if token_len > MAX_TOKEN_LENGTH {
            pgrx::warning!("There is a custom table token whose length has exceeded MAX_TOKEN_LENGTH({MAX_TOKEN_LENGTH}). It will be cut off to multiple tokens. If you need to support long token, welcome to submit an issue to \"https://github.com/tensorchord/VectorChord-bm25/issues\".");

            let replace_token = results[i][..MAX_TOKEN_LENGTH].to_string();
            let token = std::mem::replace(&mut results[i], replace_token);
            for j in 1..(token.len().div_ceil(MAX_TOKEN_LENGTH)) {
                results.push(token[j * MAX_TOKEN_LENGTH..][..MAX_TOKEN_LENGTH].to_string());
            }
        }
    }
    results
}

fn resolve_qualified_table_name(table: &str, caller_schema: &str) -> String {
    let mut parts = table.split('.');
    let first = parts.next().unwrap();
    let second = parts.next();
    if parts.next().is_some() {
        panic!("Invalid table reference: {}", table);
    }

    match second {
        Some(table_name) => format!(
            "{}.{}",
            quote_identifier(first),
            quote_identifier(table_name)
        ),
        None => format!(
            "{}.{}",
            quote_identifier(caller_schema),
            quote_identifier(first)
        ),
    }
}

fn ensure_custom_model_privileges(owner: &str, qualified_table: &str, column: &str) {
    let has_select = spi_get_one::<bool>(
        "SELECT has_column_privilege($1, $2, $3, 'SELECT')",
        &[owner.into(), qualified_table.into(), column.into()],
    )
    .unwrap_or(false);
    if !has_select {
        panic!(
            "Permission denied: role {} must have SELECT on column {} of table {}",
            owner, column, qualified_table
        );
    }

    let has_trigger = spi_get_one::<bool>(
        "SELECT has_table_privilege($1, $2, 'TRIGGER')",
        &[owner.into(), qualified_table.into()],
    )
    .unwrap_or(false);
    if !has_trigger {
        panic!(
            "Permission denied: role {} must have TRIGGER on table {}",
            owner, qualified_table
        );
    }
}

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.custom_model_insert_trigger()
RETURNS TRIGGER AS $$
DECLARE
    tokenizer_name TEXT := TG_ARGV[0];
    target_column TEXT := TG_ARGV[1];
    text_analyzer TEXT := TG_ARGV[2];
BEGIN
    EXECUTE format('
    WITH 
    new_tokens AS (
        SELECT unnest(tokenizer_catalog.apply_text_analyzer_for_custom_model($1.%I, %L)) AS token
    ),
    to_insert AS (
        SELECT token FROM new_tokens
        WHERE NOT EXISTS (
            SELECT 1 FROM tokenizer_catalog.%I WHERE token = new_tokens.token
        )
    )
    INSERT INTO tokenizer_catalog.%I (token) SELECT token FROM to_insert ON CONFLICT (token) DO NOTHING', target_column, text_analyzer, 'model_' || tokenizer_name, 'model_' || tokenizer_name) USING NEW;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog;
    "#,
    name = "custom_model_insert_trigger"
);

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.create_custom_model_tokenizer_and_trigger(
tokenizer_name TEXT, model_name TEXT, text_analyzer_name TEXT, table_name TEXT, source_column TEXT, target_column TEXT)
RETURNS VOID AS $body$
DECLARE
    table_parts TEXT[];
    qualified_table TEXT;
BEGIN
    table_parts := pg_catalog.string_to_array(table_name, '.');
    IF pg_catalog.array_length(table_parts, 1) = 1 THEN
        qualified_table := pg_catalog.format('%I.%I', pg_catalog.current_schema(), table_parts[1]);
    ELSIF pg_catalog.array_length(table_parts, 1) = 2 THEN
        qualified_table := pg_catalog.format('%I.%I', table_parts[1], table_parts[2]);
    ELSE
        RAISE EXCEPTION 'Invalid table reference: %', table_name;
    END IF;

    EXECUTE pg_catalog.format('SELECT tokenizer_catalog.create_custom_model(%L, $$
        table = %L
        column = %L
        text_analyzer = %L
        $$)', model_name, table_name, source_column, text_analyzer_name);
    EXECUTE pg_catalog.format('SELECT tokenizer_catalog.create_tokenizer(%L, $$
        text_analyzer = %L
        model = %L
        $$)', tokenizer_name, text_analyzer_name, model_name);
    EXECUTE pg_catalog.format('UPDATE %s SET %I = tokenizer_catalog.tokenize(%I, %L)', qualified_table, target_column, source_column, tokenizer_name);
    EXECUTE pg_catalog.format('CREATE TRIGGER "model_%s_trigger_insert" BEFORE INSERT OR UPDATE OF %I ON %s FOR EACH ROW EXECUTE FUNCTION tokenizer_catalog.custom_model_tokenizer_set_target_column_trigger(%L, %I, %I)', model_name, source_column, qualified_table, tokenizer_name, source_column, target_column);
END;
$body$ LANGUAGE plpgsql;
    "#,
    name = "create_custom_model_tokenizer_and_trigger"
);

pgrx::extension_sql!(
    r#"
CREATE FUNCTION tokenizer_catalog.apply_text_analyzer_for_custom_model(text TEXT, text_analyzer_name TEXT)
RETURNS TEXT[]
LANGUAGE sql VOLATILE PARALLEL SAFE SECURITY DEFINER
SET search_path = tokenizer_catalog, pg_catalog
AS $$ SELECT tokenizer_catalog.__pg_tokenizer_apply_text_analyzer_for_custom_model($1, $2); $$;
"#,
    name = "apply_text_analyzer_for_custom_model_wrapper_sql",
    requires = [apply_text_analyzer_for_custom_model_internal]
);

#[pgrx::pg_trigger]
fn custom_model_tokenizer_set_target_column_trigger<'a>(
    trigger: &'a pgrx::PgTrigger<'a>,
) -> Result<Option<PgHeapTuple<'a, impl WhoAllocated>>, ()> {
    use pgrx::IntoDatum;

    let mut new = trigger.new().expect("new tuple is missing").into_owned();
    let tg_argv = trigger.extra_args().expect("trigger arguments are missing");
    if tg_argv.len() != 3 {
        panic!("Invalid trigger arguments");
    }
    let tokenizer_name = &tg_argv[0];
    let source_column = &tg_argv[1];
    let target_column = &tg_argv[2];

    let source = new
        .get_by_name::<&str>(source_column)
        .expect("source column is missing");
    let Some(source) = source else {
        return Ok(Some(new));
    };

    let target: Vec<i32> = spi_get_one(
        "SELECT tokenizer_catalog.tokenize($1, $2)",
        &[source.into(), tokenizer_name.into()],
    )
    .expect("tokenize returned no result");
    let (idx, att) = new
        .get_attribute_by_name(target_column)
        .expect("get target column failed");
    let attoid = att.type_oid().value();
    if Vec::<i32>::is_compatible_with(attoid) {
        new.set_by_index(idx, target)
            .expect("set target column failed");
    } else {
        let target_casted = pgrx::Spi::connect(|client| {
            client
                .select(
                    &format!("SELECT $1::{}", lookup_type_name(attoid)),
                    Some(1),
                    &[target.into()],
                )
                .unwrap_or_report();

            unsafe {
                let table = pgrx::pg_sys::SPI_tuptable.as_mut().unwrap();
                if table.numvals != 1 {
                    panic!("unexpected number of tuples returned");
                }
                let heap_tuple = *(table.vals);
                let heap_tuple = pgrx::pg_sys::SPI_copytuple(heap_tuple);

                let mut is_null = false;
                let datum = pgrx::pg_sys::SPI_getbinval(heap_tuple, table.tupdesc, 1, &mut is_null);

                if is_null {
                    panic!("unexpected null value");
                }
                datum
            }
        });
        unsafe { new.set_by_index_unchecked(idx, Some(target_casted)) };
    }

    Ok(Some(new))
}

fn quote_identifier(ident: &str) -> String {
    unsafe {
        let ptr = pgrx::pg_sys::quote_identifier(ident.as_pg_cstr());
        let quoted_str = CStr::from_ptr(ptr).to_str().unwrap().to_string();
        pgrx::pg_sys::pfree(ptr as _);
        quoted_str
    }
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn lookup_type_name(oid: pgrx::pg_sys::Oid) -> String {
    unsafe {
        // SAFETY: nothing to concern ourselves with other than just calling into Postgres FFI
        // and Postgres will raise an ERROR if we pass it an invalid Oid, so it'll never return a null
        let cstr_name = pgrx::pg_sys::format_type_extended(oid, -1, 0);
        let cstr = CStr::from_ptr(cstr_name);
        let typname = cstr.to_string_lossy().to_string();
        pgrx::pg_sys::pfree(cstr_name as _); // don't leak the palloc'd cstr_name
        typname
    }
}
