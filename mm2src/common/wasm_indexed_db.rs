use crate::log::{debug, error};
use crate::mm_error::prelude::*;
use crate::{stringify_js_error, WasmUnwrapErrExt, WasmUnwrapExt};
use derive_more::Display;
use futures::channel::mpsc;
use futures::StreamExt;
use js_sys::Array;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::marker::PhantomData;
use std::sync::Mutex;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{IdbDatabase, IdbIndexParameters, IdbObjectStore, IdbObjectStoreParameters, IdbOpenDbRequest, IdbRequest,
              IdbTransaction, IdbTransactionMode, IdbVersionChangeEvent};

lazy_static! {
    static ref OPEN_DATABASES: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
}

pub type OnUpgradeResult<T> = Result<T, MmError<OnUpgradeError>>;
pub type InitDbResult<T> = Result<T, MmError<InitDbError>>;
pub type DbTransactionResult<T> = Result<T, MmError<DbTransactionError>>;

type OnUpgradeNeededCb = Box<dyn FnOnce(&DbUpgrader, u32, u32) -> OnUpgradeResult<()>>;

#[derive(Debug, Display, PartialEq)]
pub enum InitDbError {
    #[display(fmt = "Cannot initialize a Database without tables")]
    EmptyTableList,
    #[display(fmt = "Database {} is open already", db_name)]
    DbIsOpenAlready { db_name: String },
    #[display(fmt = "It seems this browser doesn't support 'IndexedDb': {}", _0)]
    NotSupported(String),
    #[display(fmt = "Invalid Database version: {}", _0)]
    InvalidVersion(String),
    #[display(fmt = "Couldn't open Database: {}", _0)]
    OpeningError(String),
    #[display(fmt = "Type mismatch: expected '{}', found '{}'", expected, found)]
    TypeMismatch { expected: String, found: String },
    #[display(fmt = "Error occurred due to an unexpected state: {:?}", _0)]
    UnexpectedState(String),
    #[display(
        fmt = "Error occurred due to the Database upgrading from {} to {} version: {}",
        old_version,
        new_version,
        error
    )]
    UpgradingError {
        old_version: u32,
        new_version: u32,
        error: OnUpgradeError,
    },
}

#[derive(Debug, Display, PartialEq)]
pub enum OnUpgradeError {
    #[display(fmt = "Error occurred due to creating the '{}' table: {}", table, description)]
    ErrorCreatingTable { table: String, description: String },
    #[display(fmt = "Error occurred due to opening the '{}' table: {}", table, description)]
    ErrorOpeningTable { table: String, description: String },
    #[display(fmt = "Error occurred due to creating the '{}' index: {}", index, description)]
    ErrorCreatingIndex { index: String, description: String },
}

#[derive(Debug, Display)]
pub enum DbTransactionError {
    #[display(fmt = "No such table '{}'", table)]
    NoSuchTable { table: String },
    #[display(fmt = "Error creating DbTransaction: {:?}", _0)]
    ErrorCreatingTransaction(String),
    #[display(fmt = "Error opening the '{}' table: {}", table, description)]
    ErrorOpeningTable { table: String, description: String },
    #[display(fmt = "Error serializing an item: {:?}", _0)]
    ErrorSerializingItem(String),
    #[display(fmt = "Error deserializing an item: {:?}", _0)]
    ErrorDeserializingItem(String),
    #[display(fmt = "Error uploading an item: {:?}", _0)]
    ErrorUploadingItem(String),
    #[display(fmt = "No such index '{}'", index)]
    NoSuchIndex { index: String },
    #[display(fmt = "Invalid index '{}': {:?}", index, description)]
    InvalidIndex { index: String, description: String },
    #[display(
        fmt = "Record not found with the specified index '{}={}': {:?}",
        index,
        index_value,
        description
    )]
    RecordNotFound {
        index: String,
        index_value: String,
        description: String,
    },
    #[display(fmt = "Error occurred due to an unexpected state: {:?}", _0)]
    UnexpectedState(String),
}

pub struct IndexedDbBuilder {
    db_name: String,
    db_version: u32,
    tables: HashMap<String, OnUpgradeNeededCb>,
}

impl IndexedDbBuilder {
    pub fn new(db_name: &str) -> IndexedDbBuilder {
        IndexedDbBuilder {
            db_name: db_name.to_owned(),
            db_version: 1,
            tables: HashMap::new(),
        }
    }

    pub fn with_version(mut self, db_version: u32) -> IndexedDbBuilder {
        self.db_version = db_version;
        self
    }

    pub fn with_table<Table: TableSignature>(mut self) -> IndexedDbBuilder {
        let on_upgrade_needed_cb = Box::new(Table::on_upgrade_needed);
        self.tables.insert(Table::table_name().to_owned(), on_upgrade_needed_cb);
        self
    }

    pub async fn init(self) -> InitDbResult<IndexedDb> {
        Self::check_if_db_is_not_open(&self.db_name)?;
        let (table_names, on_upgrade_needed_handlers) = Self::tables_into_parts(self.tables)?;

        let window = web_sys::window().expect("!window");
        let indexed_db = match window.indexed_db() {
            Ok(Some(db)) => db,
            Ok(None) => return MmError::err(InitDbError::NotSupported("Unknown error".to_owned())),
            Err(e) => return MmError::err(InitDbError::NotSupported(stringify_js_error(&e))),
        };

        let db_request = match indexed_db.open_with_u32(&self.db_name, self.db_version) {
            Ok(r) => r,
            Err(e) => return MmError::err(InitDbError::InvalidVersion(stringify_js_error(&e))),
        };
        let (tx, mut rx) = mpsc::channel(1);

        let onerror_closure = construct_event_closure(DbOpenEvent::Failed, tx.clone());
        let onsuccess_closure = construct_event_closure(DbOpenEvent::Success, tx.clone());
        let onupgradeneeded_closure = construct_event_closure(DbOpenEvent::UpgradeNeeded, tx.clone());

        db_request.set_onerror(Some(onerror_closure.as_ref().unchecked_ref()));
        db_request.set_onsuccess(Some(onsuccess_closure.as_ref().unchecked_ref()));
        db_request.set_onupgradeneeded(Some(onupgradeneeded_closure.as_ref().unchecked_ref()));

        let mut on_upgrade_needed_handlers = Some(on_upgrade_needed_handlers);
        while let Some(event) = rx.next().await {
            match event {
                DbOpenEvent::Failed(e) => return MmError::err(InitDbError::OpeningError(stringify_js_error(&e))),
                DbOpenEvent::UpgradeNeeded(event) => {
                    Self::on_upgrade_needed(event, &db_request, &mut on_upgrade_needed_handlers)?
                },
                DbOpenEvent::Success(_) => {
                    let db = Self::get_db_from_request(&db_request)?;
                    Self::cache_open_db(self.db_name.clone());

                    return Ok(IndexedDb {
                        db,
                        db_name: self.db_name,
                        tables: table_names,
                    });
                },
            }
        }
        unreachable!("The event channel must not be closed before either 'DbOpenEvent::Success' or 'DbOpenEvent::Failed' is received");
    }

    fn on_upgrade_needed(
        event: JsValue,
        db_request: &IdbOpenDbRequest,
        handlers: &mut Option<Vec<OnUpgradeNeededCb>>,
    ) -> InitDbResult<()> {
        let handlers = match handlers.take() {
            Some(handlers) => handlers,
            None => {
                return MmError::err(InitDbError::UnexpectedState(
                    "'IndexedDbBuilder::on_upgraded_needed' was called twice".to_owned(),
                ))
            },
        };

        let db = Self::get_db_from_request(&db_request)?;
        let transaction = Self::get_transaction_from_request(&db_request)?;

        let version_event = match event.dyn_into::<IdbVersionChangeEvent>() {
            Ok(version) => version,
            Err(e) => {
                return MmError::err(InitDbError::TypeMismatch {
                    expected: "IdbVersionChangeEvent".to_owned(),
                    found: format!("{:?}", e),
                })
            },
        };
        let old_version = version_event.old_version() as u32;
        let new_version = version_event
            .new_version()
            .ok_or(MmError::new(InitDbError::InvalidVersion(
                "Expected a new_version".to_owned(),
            )))? as u32;

        let upgrader = DbUpgrader { db, transaction };
        for on_upgrade_needed_cb in handlers {
            on_upgrade_needed_cb(&upgrader, old_version, new_version).mm_err(|error| InitDbError::UpgradingError {
                old_version,
                new_version,
                error,
            })?;
        }
        Ok(())
    }

    fn cache_open_db(db_name: String) {
        let mut open_databases = OPEN_DATABASES.lock().expect_w("!OPEN_DATABASES.lock()");
        open_databases.insert(db_name);
    }

    fn check_if_db_is_not_open(db_name: &str) -> InitDbResult<()> {
        let open_databases = OPEN_DATABASES.lock().expect_w("!OPEN_DATABASES.lock()");
        if open_databases.contains(db_name) {
            MmError::err(InitDbError::DbIsOpenAlready {
                db_name: db_name.to_owned(),
            })
        } else {
            Ok(())
        }
    }

    fn get_db_from_request(db_request: &IdbOpenDbRequest) -> InitDbResult<IdbDatabase> {
        let db_result = match db_request.result() {
            Ok(res) => res,
            Err(e) => return MmError::err(InitDbError::UnexpectedState(stringify_js_error(&e))),
        };
        db_result.dyn_into::<IdbDatabase>().map_err(|db_result| {
            MmError::new(InitDbError::TypeMismatch {
                expected: "IdbDatabase".to_owned(),
                found: format!("{:?}", db_result),
            })
        })
    }

    fn get_transaction_from_request(db_request: &IdbOpenDbRequest) -> InitDbResult<IdbTransaction> {
        let transaction = match db_request.transaction() {
            Some(res) => res,
            None => {
                return MmError::err(InitDbError::UnexpectedState(
                    "Expected 'IdbOpenDbRequest::transaction'".to_owned(),
                ))
            },
        };
        transaction.dyn_into::<IdbTransaction>().map_err(|transaction| {
            MmError::new(InitDbError::TypeMismatch {
                expected: "IdbTransaction".to_owned(),
                found: format!("{:?}", transaction),
            })
        })
    }

    fn tables_into_parts(
        tables: HashMap<String, OnUpgradeNeededCb>,
    ) -> InitDbResult<(HashSet<String>, Vec<OnUpgradeNeededCb>)> {
        if tables.is_empty() {
            return MmError::err(InitDbError::EmptyTableList);
        }

        let mut table_names = HashSet::with_capacity(tables.len());
        let mut on_upgrade_needed_handlers = Vec::with_capacity(tables.len());
        for (table_name, handler) in tables {
            table_names.insert(table_name);
            on_upgrade_needed_handlers.push(handler);
        }
        Ok((table_names, on_upgrade_needed_handlers))
    }
}

pub struct IndexedDb {
    db: IdbDatabase,
    db_name: String,
    tables: HashSet<String>,
}

impl fmt::Debug for IndexedDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "IndexedDb {{ db_name: {:?}, tables: {:?} }}",
            self.db_name, self.tables
        )
    }
}

/// Although wasm is currently single-threaded, we can implement the `Send` trait for `IndexedDb`,
/// but it won't be safe when wasm becomes multi-threaded.
unsafe impl Send for IndexedDb {}

impl IndexedDb {
    pub fn transaction(&self) -> DbTransactionResult<DbTransaction> {
        let store_names = Array::new();
        for table in self.tables.iter() {
            store_names.push(&JsValue::from(table));
        }

        match self
            .db
            .transaction_with_str_sequence_and_mode(&store_names, IdbTransactionMode::Readwrite)
        {
            Ok(transaction) => Ok(DbTransaction {
                transaction,
                tables: self.tables.clone(),
            }),
            Err(e) => MmError::err(DbTransactionError::ErrorCreatingTransaction(stringify_js_error(&e))),
        }
    }
}

impl Drop for IndexedDb {
    fn drop(&mut self) {
        self.db.close();
        let mut open_databases = OPEN_DATABASES.lock().expect_w("!OPEN_DATABASES.lock()");
        open_databases.remove(&self.db_name);
    }
}

pub struct DbTransaction {
    transaction: IdbTransaction,
    tables: HashSet<String>,
}

impl DbTransaction {
    pub fn open_table<Table: TableSignature>(&self) -> DbTransactionResult<DbTable<Table>> {
        let table_name = Table::table_name();
        if !self.tables.contains(table_name) {
            let table = table_name.to_owned();
            return MmError::err(DbTransactionError::NoSuchTable { table });
        }

        match self.transaction.object_store(table_name) {
            Ok(object_store) => Ok(DbTable {
                object_store,
                phantom: PhantomData::default(),
            }),
            Err(e) => MmError::err(DbTransactionError::ErrorOpeningTable {
                table: table_name.to_owned(),
                description: stringify_js_error(&e),
            }),
        }
    }
}

pub struct DbTable<'a, T: TableSignature> {
    object_store: IdbObjectStore,
    phantom: PhantomData<&'a T>,
}

impl<'a, T: TableSignature> DbTable<'a, T> {
    pub async fn add_item(&self, item: &T) -> DbTransactionResult<()> {
        let js_value = match JsValue::from_serde(item) {
            Ok(value) => value,
            Err(e) => return MmError::err(DbTransactionError::ErrorSerializingItem(e.to_string())),
        };
        let add_request = match self.object_store.add(&js_value) {
            Ok(request) => request,
            Err(e) => return MmError::err(DbTransactionError::ErrorUploadingItem(stringify_js_error(&e))),
        };

        Self::wait_for_request_complete(&add_request)
            .await
            .map(|_| ())
            .map_to_mm(|e| DbTransactionError::ErrorUploadingItem(stringify_js_error(&e)))
    }

    pub async fn get_items(&self, index_str: &str, index_value_str: &str) -> DbTransactionResult<Vec<T>> {
        let index = index_str.to_owned();
        let index_value = index_value_str.to_owned();

        let index_value_js = JsValue::from(index_value_str);

        let db_index = match self.object_store.index(index_str) {
            Ok(index) => index,
            Err(_) => return MmError::err(DbTransactionError::NoSuchIndex { index }),
        };
        let get_request = match db_index.get_all_with_key(&index_value_js) {
            Ok(request) => request,
            Err(e) => {
                return MmError::err(DbTransactionError::InvalidIndex {
                    index,
                    description: stringify_js_error(&e),
                })
            },
        };

        if let Err(e) = Self::wait_for_request_complete(&get_request).await {
            return MmError::err(DbTransactionError::RecordNotFound {
                index,
                index_value,
                description: stringify_js_error(&e),
            });
        }

        let result_js_value = match get_request.result() {
            Ok(res) => res,
            Err(e) => return MmError::err(DbTransactionError::UnexpectedState(stringify_js_error(&e))),
        };

        if result_js_value.is_null() || result_js_value.is_undefined() {
            return MmError::err(DbTransactionError::RecordNotFound {
                index,
                index_value,
                description: "Result value is null or undefined".to_owned(),
            });
        }

        match result_js_value.into_serde() {
            Ok(t) => Ok(t),
            Err(e) => MmError::err(DbTransactionError::ErrorDeserializingItem(e.to_string())),
        }
    }

    async fn wait_for_request_complete(request: &IdbRequest) -> Result<JsValue, JsValue> {
        let (tx, mut rx) = mpsc::channel(2);

        let onsuccess_closure = construct_event_closure(Ok, tx.clone());
        let onerror_closure = construct_event_closure(Err, tx.clone());

        request.set_onsuccess(Some(onsuccess_closure.as_ref().unchecked_ref()));
        request.set_onerror(Some(onerror_closure.as_ref().unchecked_ref()));

        rx.next().await.expect_w("The request event channel must not be closed")
    }
}

pub struct DbUpgrader {
    db: IdbDatabase,
    transaction: IdbTransaction,
}

impl DbUpgrader {
    pub fn create_table(&self, table: &str) -> OnUpgradeResult<TableUpgrader> {
        let mut params = IdbObjectStoreParameters::new();
        // We use the [out-of-line](https://developer.mozilla.org/en-US/docs/Web/API/IndexedDB_API/Basic_Concepts_Behind_IndexedDB#gloss_outofline_key) primary keys.
        params.auto_increment(true);

        match self.db.create_object_store_with_optional_parameters(table, &params) {
            Ok(object_store) => Ok(TableUpgrader { object_store }),
            Err(e) => MmError::err(OnUpgradeError::ErrorCreatingTable {
                table: table.to_owned(),
                description: stringify_js_error(&e),
            }),
        }
    }

    /// Open the `table` if it was created already.
    pub fn open_table(&self, table: &str) -> OnUpgradeResult<TableUpgrader> {
        match self.transaction.object_store(table) {
            Ok(object_store) => Ok(TableUpgrader { object_store }),
            Err(e) => MmError::err(OnUpgradeError::ErrorOpeningTable {
                table: table.to_owned(),
                description: stringify_js_error(&e),
            }),
        }
    }
}

pub struct TableUpgrader {
    object_store: IdbObjectStore,
}

impl TableUpgrader {
    pub fn create_index(&self, index: &str, unique: bool) -> OnUpgradeResult<()> {
        let mut params = IdbIndexParameters::new();
        params.unique(unique);
        self.object_store
            .create_index_with_str_and_optional_parameters(index, index, &params)
            .map(|_| ())
            .map_to_mm(|e| OnUpgradeError::ErrorCreatingIndex {
                index: index.to_owned(),
                description: stringify_js_error(&e),
            })
    }
}

pub trait TableSignature: DeserializeOwned + Serialize + 'static {
    fn table_name() -> &'static str;

    fn on_upgrade_needed(upgrader: &DbUpgrader, old_version: u32, new_version: u32) -> OnUpgradeResult<()>;
}

#[derive(Debug)]
enum DbOpenEvent {
    Failed(JsValue),
    UpgradeNeeded(JsValue),
    Success(JsValue),
}

/// Please note the `Event` type can be `JsValue`. It doesn't lead to a runtime error, because [`JsValue::dyn_into<JsValue>()`] returns itself.
fn construct_event_closure<F, Event>(mut f: F, mut event_tx: mpsc::Sender<Event>) -> Closure<dyn FnMut(JsValue)>
where
    F: FnMut(JsValue) -> Event + 'static,
    Event: fmt::Debug + 'static,
{
    Closure::new(move |event: JsValue| {
        let open_event = f(event);
        if let Err(e) = event_tx.try_send(open_event) {
            let error = e.to_string();
            let event = e.into_inner();
            error!("Error sending the '{:?}' event: {}", event, error);
        }
    })
}

mod tests {
    use super::*;
    use crate::for_tests::register_wasm_log;
    use crate::log::LogLevel;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    #[serde(deny_unknown_fields)]
    struct TxTable {
        ticker: String,
        tx_hash: String,
        block_height: u64,
    }

    impl TableSignature for TxTable {
        fn table_name() -> &'static str { "tx_table" }

        fn on_upgrade_needed(upgrader: &DbUpgrader, old_version: u32, _new_version: u32) -> OnUpgradeResult<()> {
            if old_version > 0 {
                // the table is initialized already
                return Ok(());
            }
            let table_upgrader = upgrader.create_table("tx_table")?;
            table_upgrader.create_index("ticker", false)?;
            table_upgrader.create_index("tx_hash", true)
        }
    }

    #[wasm_bindgen_test]
    async fn test_add_get_item() {
        const DB_NAME: &str = "TEST_ADD_GET_ITEM";
        const DB_VERSION: u32 = 1;

        let rick_tx_1 = TxTable {
            ticker: "RICK".to_owned(),
            tx_hash: "0a0fda88364b960000f445351fe7678317a1e0c80584de0413377ede00ba696f".to_owned(),
            block_height: 10000,
        };
        let rick_tx_2 = TxTable {
            ticker: "RICK".to_owned(),
            tx_hash: "ba881ecca15b5d4593f14f25debbcdfe25f101fd2e9cf8d0b5d92d19813d4424".to_owned(),
            block_height: 10000,
        };
        let morty_tx_1 = TxTable {
            ticker: "MORTY".to_owned(),
            tx_hash: "1fc789133239260ed16361190a026a88cab2243935f02f1ccd794f1d06a22246".to_owned(),
            block_height: 20000,
        };
        let morty_tx_1_updated = TxTable {
            ticker: "MORTY".to_owned(),
            tx_hash: "1fc789133239260ed16361190a026a88cab2243935f02f1ccd794f1d06a22246".to_owned(),
            block_height: 30000,
        };

        register_wasm_log(LogLevel::Debug);

        let db = IndexedDbBuilder::new(DB_NAME)
            .with_version(DB_VERSION)
            .with_table::<TxTable>()
            .init()
            .await
            .expect_w("!IndexedDb::init");
        let transaction = db.transaction().expect_w("!IndexedDb::transaction()");
        let table = transaction
            .open_table::<TxTable>()
            .expect_w("!DbTransaction::open_table");

        table
            .add_item(&rick_tx_1)
            .await
            .expect_w("!Couldn't add a 'RICK' transaction");
        table
            .add_item(&rick_tx_2)
            .await
            .expect_w("!Couldn't add a 'RICK' transaction with the different 'tx_hash'");
        table
            .add_item(&morty_tx_1)
            .await
            .expect_w("!Couldn't add a 'MORTY' transaction");

        let actual_rick_txs = table
            .get_items("ticker", "RICK")
            .await
            .expect_w("!Couldn't get items by the index 'ticker=RICK'");
        assert_eq!(actual_rick_txs, vec![rick_tx_1, rick_tx_2.clone()]);
        let actual_rick_2_tx = table
            .get_items(
                "tx_hash",
                "ba881ecca15b5d4593f14f25debbcdfe25f101fd2e9cf8d0b5d92d19813d4424",
            )
            .await
            .expect_w("!Couldn't get items by the index 'tx_hash'");
        assert_eq!(actual_rick_2_tx, vec![rick_tx_2]);

        // Try to add the same item. [`TxTable::tx_hash`] is a unique index, so this operation must fail.
        let err = table
            .add_item(&morty_tx_1_updated)
            .await
            .expect_err_w("!Couldn't add an item with the different 'tx_hash'");
        match err.into_inner() {
            DbTransactionError::ErrorUploadingItem(err) => debug!("error: {}", err),
            e => panic!("Expected 'DbTransactionError::ErrorUploadingItem', found: {:?}", e),
        }

        // TODO replace morty_tx_1_updated
    }

    #[wasm_bindgen_test]
    async fn test_upgrade_needed() {
        const DB_NAME: &str = "TEST_UPGRADE_NEEDED";

        lazy_static! {
            static ref LAST_VERSIONS: Mutex<Option<(u32, u32)>> = Mutex::new(None);
        }

        #[derive(Serialize, Deserialize)]
        struct UpgradableTable;

        impl TableSignature for UpgradableTable {
            fn table_name() -> &'static str { "upgradable_table" }

            fn on_upgrade_needed(upgrader: &DbUpgrader, old_version: u32, new_version: u32) -> OnUpgradeResult<()> {
                let mut versions = LAST_VERSIONS.lock().expect_w("!old_new_versions.lock()");
                *versions = Some((old_version, new_version));

                match (old_version, new_version) {
                    (0, 1) => {
                        let table = upgrader.create_table("upgradable_table")?;
                        table.create_index("first_index", false)?;
                    },
                    (0, 2) => {
                        let table = upgrader.create_table("upgradable_table")?;
                        table.create_index("first_index", false)?;
                        table.create_index("second_index", false)?;
                    },
                    (1, 2) => {
                        let table = upgrader.open_table("upgradable_table")?;
                        table.create_index("second_index", false)?;
                    },
                    v => panic!("Unexpected old, new versions: {:?}", v),
                }
                Ok(())
            }
        }

        async fn init_and_check(version: u32, expected_old_new_versions: Option<(u32, u32)>) -> Result<(), String> {
            let mut versions = LAST_VERSIONS.lock().expect_w("!LAST_VERSIONS.lock()");
            *versions = None;
            drop(versions);

            let _db = IndexedDbBuilder::new(DB_NAME)
                .with_version(version)
                .with_table::<UpgradableTable>()
                .init()
                .await
                .map_err(|e| format!("{}", e))?;

            let actual_versions = LAST_VERSIONS.lock().unwrap_w();
            if *actual_versions == expected_old_new_versions {
                Ok(())
            } else {
                Err(format!(
                    "Expected {:?}, found {:?}",
                    expected_old_new_versions, actual_versions
                ))
            }
        }

        register_wasm_log(LogLevel::Debug);

        init_and_check(1, Some((0, 1))).await.unwrap_w();
        init_and_check(2, Some((1, 2))).await.unwrap_w();
        // the same 2 version, `on_upgrade_needed` must not be called
        init_and_check(2, None).await.unwrap_w();
    }

    #[wasm_bindgen_test]
    async fn test_open_twice() {
        const DB_NAME: &str = "TEST_OPEN_TWICE";
        const DB_VERSION: u32 = 1;

        register_wasm_log(LogLevel::Debug);

        let _db = IndexedDbBuilder::new(DB_NAME)
            .with_version(DB_VERSION)
            .with_table::<TxTable>()
            .init()
            .await
            .expect_w("!IndexedDb::init first time");

        let err = IndexedDbBuilder::new(DB_NAME)
            .with_version(DB_VERSION + 1)
            .with_table::<TxTable>()
            .init()
            .await
            .expect_err_w("!IndexedDb::init should have failed");
        assert_eq!(err.into_inner(), InitDbError::DbIsOpenAlready {
            db_name: DB_NAME.to_owned()
        });
    }

    #[wasm_bindgen_test]
    async fn test_open_close_and_open() {
        const DB_NAME: &str = "TEST_OPEN_CLOSE_AND_OPEN";
        const DB_VERSION: u32 = 1;

        register_wasm_log(LogLevel::Debug);

        let db = IndexedDbBuilder::new(DB_NAME)
            .with_version(DB_VERSION)
            .with_table::<TxTable>()
            .init()
            .await
            .expect_w("!IndexedDb::init first time");
        drop(db);

        let _db = IndexedDbBuilder::new(DB_NAME)
            .with_version(DB_VERSION)
            .with_table::<TxTable>()
            .init()
            .await
            .expect_w("!IndexedDb::init second time");
    }
}
