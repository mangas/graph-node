//! Support for storing the entities of a subgraph in a relational schema,
//! i.e., one where each entity type gets its own table, and the table
//! structure follows the structure of the GraphQL type, using the
//! native SQL types that most appropriately map to the corresponding
//! GraphQL types
//!
//! The pivotal struct in this module is the `Layout` which handles all the
//! information about mapping a GraphQL schema to database tables

mod ddl;

#[cfg(test)]
mod ddl_tests;
#[cfg(test)]
mod query_tests;

pub(crate) mod dsl;
pub(crate) mod index;
mod prune;
mod rollup;
pub(crate) mod value;

use diesel::deserialize::FromSql;
use diesel::pg::Pg;
use diesel::serialize::{Output, ToSql};
use diesel::sql_types::Text;
use diesel::{connection::SimpleConnection, Connection};
use diesel::{
    debug_query, sql_query, OptionalExtension, PgConnection, QueryDsl, QueryResult, RunQueryDsl,
};
use graph::blockchain::block_stream::{EntityOperationKind, EntitySourceOperation};
use graph::blockchain::BlockTime;
use graph::cheap_clone::CheapClone;
use graph::components::store::write::{RowGroup, WriteChunk};
use graph::data::graphql::TypeExt as _;
use graph::data::query::Trace;
use graph::data::value::Word;
use graph::data_source::CausalityRegion;
use graph::internal_error;
use graph::prelude::{q, EntityQuery, StopwatchMetrics, ENV_VARS};
use graph::schema::{
    EntityKey, EntityType, Field, FulltextConfig, FulltextDefinition, InputSchema,
};
use graph::slog::warn;
use index::IndexList;
use inflector::Inflector;
use itertools::Itertools;
use lazy_static::lazy_static;
use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::{From, TryFrom};
use std::fmt::{self, Write};
use std::ops::Range;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::relational::value::{FromOidRow, OidRow};
use crate::relational_queries::{
    ConflictingEntitiesData, ConflictingEntitiesQuery, EntityDataExt, FindChangesQuery,
    FindDerivedQuery, FindPossibleDeletionsQuery, ReturnedEntityData,
};
use crate::{
    primary::{Namespace, Site},
    relational_queries::{
        ClampRangeQuery, EntityData, EntityDeletion, FilterCollection, FilterQuery, FindManyQuery,
        FindRangeQuery, InsertQuery, RevertClampQuery, RevertRemoveQuery,
    },
};
use graph::components::store::{AttributeNames, DerivedEntityQuery};
use graph::data::store::{IdList, IdType, BYTES_SCALAR};
use graph::data::subgraph::schema::POI_TABLE;
use graph::prelude::{
    anyhow, info, BlockNumber, DeploymentHash, Entity, EntityOperation, Logger,
    QueryExecutionError, StoreError, ValueType,
};

use crate::block_range::{BoundSide, BLOCK_COLUMN, BLOCK_RANGE_COLUMN};
pub use crate::catalog::Catalog;
use crate::ForeignServer;
use crate::{catalog, deployment};

use self::rollup::Rollup;

const DELETE_OPERATION_CHUNK_SIZE: usize = 1_000;

/// The size of string prefixes that we index. This is chosen so that we
/// will index strings that people will do string comparisons like
/// `=` or `!=` on; if text longer than this is stored in a String attribute
/// it is highly unlikely that they will be used for exact string operations.
/// This also makes sure that we do not put strings into a BTree index that's
/// bigger than Postgres' limit on such strings which is about 2k
pub const STRING_PREFIX_SIZE: usize = 256;
pub const BYTE_ARRAY_PREFIX_SIZE: usize = 64;

lazy_static! {
    static ref STATEMENT_TIMEOUT: Option<String> = ENV_VARS
        .graphql
        .sql_statement_timeout
        .map(|duration| format!("set local statement_timeout={}", duration.as_millis()));
}

/// A string we use as a SQL name for a table or column. The important thing
/// is that SQL names are snake cased. Using this type makes it easier to
/// spot cases where we use a GraphQL name like 'bigThing' when we should
/// really use the SQL version 'big_thing'
///
/// We use `SqlName` for example for table and column names, and feed these
/// directly to Postgres. Postgres truncates names to 63 characters; if
/// users have GraphQL type names that do not differ in the first
/// 63 characters after snakecasing, schema creation will fail because, to
/// Postgres, we would create the same table twice. We consider this case
/// to be pathological and so unlikely in practice that we do not try to work
/// around it in the application.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Hash, Ord)]
pub struct SqlName(String);

impl SqlName {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn quoted(&self) -> String {
        format!("\"{}\"", self.0)
    }

    // Check that `name` matches the regular expression `/[A-Za-z][A-Za-z0-9_]*/`
    // without pulling in a regex matcher
    pub fn check_valid_identifier(name: &str, kind: &str) -> Result<(), StoreError> {
        let mut chars = name.chars();
        match chars.next() {
            Some(c) => {
                if !c.is_ascii_alphabetic() && c != '_' {
                    let msg = format!(
                        "the name `{}` can not be used for a {}; \
                         it must start with an ASCII alphabetic character or `_`",
                        name, kind
                    );
                    return Err(StoreError::InvalidIdentifier(msg));
                }
            }
            None => {
                let msg = format!("can not use an empty name for a {}", kind);
                return Err(StoreError::InvalidIdentifier(msg));
            }
        }
        for c in chars {
            if !c.is_ascii_alphanumeric() && c != '_' {
                let msg = format!(
                    "the name `{}` can not be used for a {}; \
                     it can only contain alphanumeric characters and `_`",
                    name, kind
                );
                return Err(StoreError::InvalidIdentifier(msg));
            }
        }
        Ok(())
    }

    pub fn verbatim(s: String) -> Self {
        SqlName(s)
    }

    pub fn qualified_name(namespace: &Namespace, name: &SqlName) -> Self {
        SqlName(format!("\"{}\".\"{}\"", namespace, name.as_str()))
    }
}

impl From<&str> for SqlName {
    fn from(name: &str) -> Self {
        SqlName(name.to_snake_case())
    }
}

impl From<String> for SqlName {
    fn from(name: String) -> Self {
        SqlName(name.to_snake_case())
    }
}

impl From<SqlName> for Word {
    fn from(name: SqlName) -> Self {
        Word::from(name.0)
    }
}

impl fmt::Display for SqlName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Borrow<str> for &SqlName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for SqlName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl FromSql<Text, Pg> for SqlName {
    fn from_sql(bytes: diesel::pg::PgValue) -> diesel::deserialize::Result<Self> {
        <String as FromSql<Text, Pg>>::from_sql(bytes).map(|s| SqlName::verbatim(s))
    }
}

impl ToSql<Text, Pg> for SqlName {
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Pg>) -> diesel::serialize::Result {
        <String as ToSql<Text, Pg>>::to_sql(&self.0, out)
    }
}

impl std::ops::Deref for SqlName {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

#[derive(Debug, Clone)]
pub struct Layout {
    /// Details of where the subgraph is stored
    pub site: Arc<Site>,
    /// Maps the GraphQL name of a type to the relational table
    pub tables: HashMap<EntityType, Arc<Table>>,
    /// The database schema for this subgraph
    pub catalog: Catalog,
    /// How many blocks of history the subgraph should keep
    pub history_blocks: BlockNumber,

    pub input_schema: InputSchema,

    /// The rollups for aggregations in this layout
    rollups: Vec<Rollup>,
}

impl Layout {
    /// Generate a layout for a relational schema for entities in the
    /// GraphQL schema `schema`. The name of the database schema in which
    /// the subgraph's tables live is in `site`.
    pub fn new(
        site: Arc<Site>,
        schema: &InputSchema,
        catalog: Catalog,
    ) -> Result<Self, StoreError> {
        // Check that enum type names are valid for SQL
        for name in schema.enum_types() {
            SqlName::check_valid_identifier(name, "enum")?;
        }

        // Construct a Table struct for each entity type, except for PoI
        // since we handle that specially
        let entity_tables = schema.entity_types();
        let ts_tables = schema.ts_entity_types();
        let has_ts_tables = !ts_tables.is_empty();

        let mut tables = entity_tables
            .iter()
            .chain(ts_tables.iter())
            .enumerate()
            .map(|(i, entity_type)| {
                Table::new(
                    schema,
                    entity_type,
                    &catalog,
                    schema
                        .entity_fulltext_definitions(entity_type.as_str())
                        .map_err(|_| StoreError::FulltextSearchNonDeterministic)?,
                    i as u32,
                    catalog.entities_with_causality_region.contains(entity_type),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        // Construct tables for timeseries

        if catalog.use_poi {
            tables.push(Self::make_poi_table(
                &schema,
                &catalog,
                has_ts_tables,
                tables.len(),
            ))
        }

        let tables: HashMap<_, _> = tables
            .into_iter()
            .fold(HashMap::new(), |mut tables, table| {
                tables.insert(table.object.clone(), Arc::new(table));
                tables
            });

        let rollups = Self::rollups(&tables, &schema)?;

        Ok(Layout {
            site,
            catalog,
            tables,
            history_blocks: i32::MAX,
            input_schema: schema.cheap_clone(),
            rollups,
        })
    }

    fn make_poi_table(
        schema: &InputSchema,
        catalog: &Catalog,
        has_ts_tables: bool,
        position: usize,
    ) -> Table {
        let poi_type = schema.poi_type();
        let poi_digest = schema.poi_digest();
        let poi_block_time = schema.poi_block_time();

        let mut columns = vec![
            Column {
                name: SqlName::from(poi_digest.as_str()),
                field: poi_digest,
                field_type: q::Type::NonNullType(Box::new(q::Type::NamedType(
                    BYTES_SCALAR.to_owned(),
                ))),
                column_type: ColumnType::Bytes,
                fulltext_fields: None,
                is_reference: false,
                use_prefix_comparison: false,
            },
            Column {
                name: SqlName::from(PRIMARY_KEY_COLUMN),
                field: Word::from(PRIMARY_KEY_COLUMN),
                field_type: q::Type::NonNullType(Box::new(q::Type::NamedType("String".to_owned()))),
                column_type: ColumnType::String,
                fulltext_fields: None,
                is_reference: false,
                use_prefix_comparison: false,
            },
        ];

        // If the subgraph uses timeseries, store the block time in the PoI
        // table
        if has_ts_tables {
            // FIXME: Use `Timestamp` as the field type when that's
            // available
            let ts_column = Column {
                name: SqlName::from(poi_block_time.as_str()),
                field: poi_block_time,
                field_type: q::Type::NonNullType(Box::new(q::Type::NamedType("Int8".to_owned()))),
                column_type: ColumnType::Int8,
                fulltext_fields: None,
                is_reference: false,
                use_prefix_comparison: false,
            };
            columns.push(ts_column);
        }

        let table_name = SqlName::verbatim(POI_TABLE.to_owned());
        let nsp = catalog.site.namespace.clone();
        Table {
            object: poi_type.to_owned(),
            qualified_name: SqlName::qualified_name(&catalog.site.namespace, &table_name),
            nsp,
            name: table_name,
            columns,
            // The position of this table in all the tables for this layout; this
            // is really only needed for the tests to make the names of indexes
            // predictable
            position: position as u32,
            is_account_like: false,
            immutable: false,
            has_causality_region: false,
        }
    }

    pub fn create_relational_schema(
        conn: &mut PgConnection,
        site: Arc<Site>,
        schema: &InputSchema,
        entities_with_causality_region: BTreeSet<EntityType>,
        index_def: Option<IndexList>,
    ) -> Result<Layout, StoreError> {
        let catalog =
            Catalog::for_creation(conn, site.cheap_clone(), entities_with_causality_region)?;
        let layout = Self::new(site, schema, catalog)?;
        let sql = layout
            .as_ddl(index_def)
            .map_err(|_| StoreError::Unknown(anyhow!("failed to generate DDL for layout")))?;
        conn.batch_execute(&sql)?;
        Ok(layout)
    }

    /// Determine if it is possible to copy the data of `source` into `self`
    /// by checking that our schema is compatible with `source`.
    /// Returns a list of errors if copying is not possible. An empty
    /// vector indicates that copying is possible
    pub fn can_copy_from(&self, base: &Layout) -> Vec<String> {
        // We allow both not copying tables at all from the source, as well
        // as adding new tables in `self`; we only need to check that tables
        // that actually need to be copied from the source are compatible
        // with the corresponding tables in `self`
        self.tables
            .values()
            .filter_map(|dst| base.table(&dst.name).map(|src| (dst, src)))
            .flat_map(|(dst, src)| dst.can_copy_from(src))
            .collect()
    }

    /// Import the database schema for this layout from its own database
    /// shard (in `self.site.shard`) into the database represented by `conn`
    /// if the schema for this layout does not exist yet
    pub fn import_schema(&self, conn: &mut PgConnection) -> Result<(), StoreError> {
        let make_query = || -> Result<String, fmt::Error> {
            let nsp = self.site.namespace.as_str();
            let srvname = ForeignServer::name(&self.site.shard);

            let mut query = String::new();
            writeln!(query, "create schema {};", nsp)?;
            // Postgres does not import enums. We recreate them in the target
            // database, otherwise importing tables that use them fails
            self.write_enum_ddl(&mut query)?;
            writeln!(
                query,
                "import foreign schema {nsp} from server {srvname} into {nsp}",
                nsp = nsp,
                srvname = srvname
            )?;
            Ok(query)
        };

        if !catalog::has_namespace(conn, &self.site.namespace)? {
            let query = make_query().map_err(|_| {
                StoreError::Unknown(anyhow!(
                    "failed to generate SQL to import foreign schema {}",
                    self.site.namespace
                ))
            })?;

            conn.batch_execute(&query)?;
        }
        Ok(())
    }

    /// Find the table with the provided `name`. The name must exactly match
    /// the name of an existing table. No conversions of the name are done
    pub fn table(&self, name: &SqlName) -> Option<&Table> {
        self.tables
            .values()
            .find(|table| &table.name == name)
            .map(|rc| rc.as_ref())
    }

    pub fn table_for_entity(&self, entity: &EntityType) -> Result<&Arc<Table>, StoreError> {
        self.tables
            .get(entity)
            .ok_or_else(|| StoreError::UnknownTable(entity.to_string()))
    }

    pub fn find(
        &self,
        conn: &mut PgConnection,
        key: &EntityKey,
        block: BlockNumber,
    ) -> Result<Option<Entity>, StoreError> {
        let table = self.table_for_entity(&key.entity_type)?.dsl_table();
        let columns = table.selected_columns::<Entity>(&AttributeNames::All, None)?;

        let query = table
            .select_cols(&columns)
            .filter(table.id_eq(&key.entity_id))
            .filter(table.at_block(block))
            .filter(table.belongs_to_causality_region(key.causality_region));

        query
            .get_result::<OidRow>(conn)
            .optional()?
            .map(|row| Entity::from_oid_row(row, &self.input_schema, &columns))
            .transpose()
    }

    // An optimization when looking up multiple entities, it will generate a single sql query using `UNION ALL`.
    pub fn find_many(
        &self,
        conn: &mut PgConnection,
        ids_for_type: &BTreeMap<(EntityType, CausalityRegion), IdList>,
        block: BlockNumber,
    ) -> Result<BTreeMap<EntityKey, Entity>, StoreError> {
        if ids_for_type.is_empty() {
            return Ok(BTreeMap::new());
        }

        let mut tables = Vec::new();
        for (entity_type, cr) in ids_for_type.keys() {
            tables.push((self.table_for_entity(entity_type)?.as_ref(), *cr));
        }
        let query = FindManyQuery::new(tables, ids_for_type, block);
        let mut entities: BTreeMap<EntityKey, Entity> = BTreeMap::new();
        for data in query.load::<EntityData>(conn)? {
            let entity_type = data.entity_type(&self.input_schema);
            let entity_data: Entity = data.deserialize_with_layout(self, None)?;

            let key =
                entity_type.key_in(entity_data.id(), CausalityRegion::from_entity(&entity_data));
            if entities.contains_key(&key) {
                return Err(internal_error!(
                    "duplicate entity {}[{}] in result set, block = {}",
                    key.entity_type,
                    key.entity_id,
                    block
                ));
            } else {
                entities.insert(key, entity_data);
            }
        }
        Ok(entities)
    }

    pub fn find_range(
        &self,
        conn: &mut PgConnection,
        entity_types: Vec<EntityType>,
        causality_region: CausalityRegion,
        block_range: Range<BlockNumber>,
    ) -> Result<BTreeMap<BlockNumber, Vec<EntitySourceOperation>>, StoreError> {
        let mut tables = vec![];
        for et in entity_types {
            tables.push(self.table_for_entity(&et)?.as_ref());
        }
        let mut entities: BTreeMap<BlockNumber, Vec<EntitySourceOperation>> = BTreeMap::new();

        // Collect all entities that have their 'lower(block_range)' attribute in the
        // interval of blocks defined by the variable block_range. For the immutable
        // entities the respective attribute is 'block$'.
        // Here are all entities that are created or modified in the block_range.
        let lower_vec = FindRangeQuery::new(
            &tables,
            causality_region,
            BoundSide::Lower,
            block_range.clone(),
        )
        .get_results::<EntityDataExt>(conn)
        .optional()?
        .unwrap_or_default();
        // Collect all entities that have their 'upper(block_range)' attribute in the
        // interval of blocks defined by the variable block_range. For the immutable
        // entities no entries are returned.
        // Here are all entities that are modified or deleted in the block_range,
        // but will have the previous versions, i.e. in the case of an update, it's
        // the version before the update, and lower_vec will have a corresponding
        // entry with the new version.
        let upper_vec =
            FindRangeQuery::new(&tables, causality_region, BoundSide::Upper, block_range)
                .get_results::<EntityDataExt>(conn)
                .optional()?
                .unwrap_or_default();
        let mut lower_iter = lower_vec.iter().fuse().peekable();
        let mut upper_iter = upper_vec.iter().fuse().peekable();
        let mut lower_now = lower_iter.next();
        let mut upper_now = upper_iter.next();
        // A closure to convert the entity data from the database into entity operation.
        let transform = |ede: &EntityDataExt,
                         entity_op: EntityOperationKind|
         -> Result<(EntitySourceOperation, BlockNumber), StoreError> {
            let e = EntityData::new(ede.entity.clone(), ede.data.clone());
            let block = ede.block_number;
            let entity_type = e.entity_type(&self.input_schema);
            let entity = e.deserialize_with_layout::<Entity>(self, None)?;
            let vid = ede.vid;
            let ewt = EntitySourceOperation {
                entity_op,
                entity_type,
                entity,
                vid,
            };
            Ok((ewt, block))
        };

        fn compare_entity_data_ext(a: &EntityDataExt, b: &EntityDataExt) -> std::cmp::Ordering {
            a.block_number
                .cmp(&b.block_number)
                .then_with(|| a.entity.cmp(&b.entity))
                .then_with(|| a.id.cmp(&b.id))
        }

        // The algorithm is a similar to merge sort algorithm and it relays on the fact that both vectors
        // are ordered by (block_number, entity_type, entity_id). It advances simultaneously entities from
        // both lower_vec and upper_vec and tries to match entities that have entries in both vectors for
        // a particular block. The match is successful if an entry in one array has the same values in the
        // other one for the number of the block, entity type and the entity id. The comparison operation
        // over the EntityDataExt implements that check. If there is a match it’s a modification operation,
        // since both sides of a range are present for that block, entity type and id. If one side of the
        // range exists and the other is missing it is a creation or deletion depending on which side is
        // present. For immutable entities the entries in upper_vec are missing, hence they are considered
        // having a lower bound at particular block and upper bound at infinity.
        while lower_now.is_some() || upper_now.is_some() {
            let (ewt, block) = match (lower_now, upper_now) {
                (Some(lower), Some(upper)) => {
                    match compare_entity_data_ext(lower, upper) {
                        std::cmp::Ordering::Greater => {
                            // we have upper bound at this block, but no lower bounds at the same block so it's deletion
                            let (ewt, block) = transform(upper, EntityOperationKind::Delete)?;
                            // advance upper_vec pointer
                            upper_now = upper_iter.next();
                            (ewt, block)
                        }
                        std::cmp::Ordering::Less => {
                            // we have lower bound at this block but no upper bound at the same block so its creation
                            let (ewt, block) = transform(lower, EntityOperationKind::Create)?;
                            // advance lower_vec pointer
                            lower_now = lower_iter.next();
                            (ewt, block)
                        }
                        std::cmp::Ordering::Equal => {
                            let (ewt, block) = transform(lower, EntityOperationKind::Modify)?;
                            // advance both lower_vec and upper_vec pointers
                            lower_now = lower_iter.next();
                            upper_now = upper_iter.next();
                            (ewt, block)
                        }
                    }
                }
                (Some(lower), None) => {
                    // we have lower bound at this block but no upper bound at the same block so its creation
                    let (ewt, block) = transform(lower, EntityOperationKind::Create)?;
                    // advance lower_vec pointer
                    lower_now = lower_iter.next();
                    (ewt, block)
                }
                (None, Some(upper)) => {
                    // we have upper bound at this block, but no lower bounds at all so it's deletion
                    let (ewt, block) = transform(upper, EntityOperationKind::Delete)?;
                    // advance upper_vec pointer
                    upper_now = upper_iter.next();
                    (ewt, block)
                }
                _ => panic!("Imposible case to happen"),
            };

            match entities.get_mut(&block) {
                Some(vec) => vec.push(ewt),
                None => {
                    let _ = entities.insert(block, vec![ewt]);
                }
            };
        }

        // sort the elements in each blocks bucket by vid
        for (_, vec) in &mut entities {
            vec.sort_by(|a, b| a.vid.cmp(&b.vid));
        }

        Ok(entities)
    }

    pub fn find_derived(
        &self,
        conn: &mut PgConnection,
        derived_query: &DerivedEntityQuery,
        block: BlockNumber,
        excluded_keys: &Vec<EntityKey>,
    ) -> Result<BTreeMap<EntityKey, Entity>, StoreError> {
        let table = self.table_for_entity(&derived_query.entity_type)?;
        let ids = excluded_keys.iter().map(|key| &key.entity_id).cloned();
        let excluded_keys = IdList::try_from_iter(derived_query.entity_type.id_type()?, ids)?;
        let query = FindDerivedQuery::new(table, derived_query, block, excluded_keys);

        let mut entities = BTreeMap::new();

        for data in query.load::<EntityData>(conn)? {
            let entity_type = data.entity_type(&self.input_schema);
            let entity_data: Entity = data.deserialize_with_layout(self, None)?;
            let key =
                entity_type.key_in(entity_data.id(), CausalityRegion::from_entity(&entity_data));

            entities.insert(key, entity_data);
        }
        Ok(entities)
    }

    pub fn find_changes(
        &self,
        conn: &mut PgConnection,
        block: BlockNumber,
    ) -> Result<Vec<EntityOperation>, StoreError> {
        let mut tables = Vec::new();
        for table in self.tables.values() {
            if table.name.as_str() != POI_TABLE {
                tables.push(&**table);
            }
        }

        let inserts_or_updates =
            FindChangesQuery::new(&tables[..], block).load::<EntityData>(conn)?;
        let deletions =
            FindPossibleDeletionsQuery::new(&tables[..], block).load::<EntityDeletion>(conn)?;

        let mut processed_entities = HashSet::new();
        let mut changes = Vec::new();

        for entity_data in inserts_or_updates.into_iter() {
            let entity_type = entity_data.entity_type(&self.input_schema);
            let data: Entity = entity_data.deserialize_with_layout(self, None)?;
            let entity_id = data.id();
            processed_entities.insert((entity_type.clone(), entity_id.clone()));

            changes.push(EntityOperation::Set {
                key: entity_type.key_in(entity_id, CausalityRegion::from_entity(&data)),
                data,
            });
        }

        for del in &deletions {
            let entity_type = del.entity_type(&self.input_schema);

            // See the doc comment of `FindPossibleDeletionsQuery` for details
            // about why this check is necessary.
            let entity_id = entity_type.parse_id(del.id())?;
            if !processed_entities.contains(&(entity_type.clone(), entity_id.clone())) {
                changes.push(EntityOperation::Remove {
                    key: entity_type.key_in(entity_id, del.causality_region()),
                });
            }
        }

        Ok(changes)
    }

    pub fn insert<'a>(
        &'a self,
        conn: &mut PgConnection,
        group: &'a RowGroup,
        stopwatch: &StopwatchMetrics,
    ) -> Result<(), StoreError> {
        fn chunk_details(chunk: &WriteChunk) -> (BlockNumber, String) {
            let count = chunk.len();
            let first = chunk.iter().map(|row| row.block).min().unwrap_or(0);
            let last = chunk.iter().map(|row| row.block).max().unwrap_or(0);
            let ids = if chunk.len() < 20 {
                format!(
                    " with ids [{}]",
                    chunk.iter().map(|row| row.to_string()).join(", ")
                )
            } else {
                "".to_string()
            };
            let details = if first == last {
                format!("insert {count} rows{ids}")
            } else {
                format!("insert {count} rows at blocks [{first}, {last}]{ids}")
            };
            (last, details)
        }

        let table = self.table_for_entity(&group.entity_type)?;
        let _section = stopwatch.start_section("insert_modification_insert_query");

        // We insert the entities in chunks to make sure each operation does
        // not exceed the maximum number of bindings allowed in queries
        let chunk_size = InsertQuery::chunk_size(table);
        for chunk in group.write_chunks(chunk_size) {
            // Empty chunks would lead to invalid SQL
            if !chunk.is_empty() {
                InsertQuery::new(table, &chunk)?
                    .execute(conn)
                    .map_err(|e| {
                        let (block, msg) = chunk_details(&chunk);
                        StoreError::write_failure(e, table.object.as_str(), block, msg)
                    })?;
            }
        }
        Ok(())
    }

    pub fn conflicting_entities(
        &self,
        conn: &mut PgConnection,
        entities: &[EntityType],
        group: &RowGroup,
    ) -> Result<Option<(String, String)>, StoreError> {
        Ok(ConflictingEntitiesQuery::new(self, entities, group)?
            .load(conn)?
            .pop()
            .map(|data: ConflictingEntitiesData| (data.entity, data.id)))
    }

    /// order is a tuple (attribute, value_type, direction)
    pub fn query<T: crate::relational_queries::FromEntityData>(
        &self,
        logger: &Logger,
        conn: &mut PgConnection,
        query: EntityQuery,
    ) -> Result<(Vec<T>, Trace), QueryExecutionError> {
        fn log_query_timing(
            logger: &Logger,
            query: &FilterQuery,
            elapsed: Duration,
            entity_count: usize,
            trace: bool,
        ) -> Trace {
            // 20kB
            const MAXLEN: usize = 20_480;

            if !ENV_VARS.log_sql_timing() && !trace {
                return Trace::None;
            }

            let mut text = debug_query(&query).to_string().replace('\n', "\t");

            let trace = if trace {
                Trace::query(&text, elapsed, entity_count)
            } else {
                Trace::None
            };

            if ENV_VARS.log_sql_timing() {
                // If the query + bind variables is more than MAXLEN, truncate it;
                // this will happen when queries have very large bind variables
                // (e.g., long arrays of string ids)
                if text.len() > MAXLEN {
                    text.truncate(MAXLEN);
                    text.push_str(" ...");
                }
                info!(
                    logger,
                    "Query timing (SQL)";
                    "query" => text,
                    "time_ms" => elapsed.as_millis(),
                    "entity_count" => entity_count
                );
            }
            trace
        }

        let trace = query.trace;

        let filter_collection =
            FilterCollection::new(self, query.collection, query.filter.as_ref(), query.block)?;
        let query = FilterQuery::new(
            &filter_collection,
            self,
            query.filter.as_ref(),
            query.order,
            query.range,
            query.block,
            query.query_id,
            &self.site,
        )?;

        let query_clone = query.clone();

        let start = Instant::now();
        let values = conn
            .transaction(|conn| {
                if let Some(ref timeout_sql) = *STATEMENT_TIMEOUT {
                    conn.batch_execute(timeout_sql)?;
                }
                query.load::<EntityData>(conn)
            })
            .map_err(|e| {
                use diesel::result::DatabaseErrorKind;
                use diesel::result::Error::*;
                // Sometimes `debug_query(..)` can't be turned into a
                // string, e.g., because `walk_ast` for one of its fragments
                // returns an error. When that happens, avoid a panic from
                // simply calling `to_string()` on it, and output a string
                // representation of the `FilterQuery` instead of the SQL
                let mut query_text = String::new();
                match write!(query_text, "{}", debug_query(&query_clone)) {
                    Ok(()) => (),
                    Err(_) => {
                        write!(query_text, "{query_clone}").ok();
                    }
                };
                match e {
                    DatabaseError(DatabaseErrorKind::Unknown, ref info)
                        if info.message().starts_with("syntax error in tsquery") =>
                    {
                        QueryExecutionError::FulltextQueryInvalidSyntax(info.message().to_string())
                    }
                    _ => QueryExecutionError::ResolveEntitiesError(format!(
                        "{e}, query = {query_text}",
                    )),
                }
            })?;
        let trace = log_query_timing(logger, &query_clone, start.elapsed(), values.len(), trace);

        let parent_type = filter_collection.parent_type()?.map(ColumnType::from);
        values
            .into_iter()
            .map(|entity_data| {
                entity_data
                    .deserialize_with_layout(self, parent_type.as_ref())
                    .map_err(|e| e.into())
            })
            .collect::<Result<Vec<T>, _>>()
            .map(|values| (values, trace))
    }

    pub fn update<'a>(
        &'a self,
        conn: &mut PgConnection,
        group: &'a RowGroup,
        stopwatch: &StopwatchMetrics,
    ) -> Result<usize, StoreError> {
        let table = self.table_for_entity(&group.entity_type)?;
        if table.immutable && group.has_clamps() {
            let ids = group
                .ids()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(internal_error!(
                "entities of type `{}` can not be updated since they are immutable. Entity ids are [{}]",
                group.entity_type,
                ids
            ));
        }

        let section = stopwatch.start_section("update_modification_clamp_range_query");
        for (block, rows) in group.clamps_by_block() {
            let entity_keys: Vec<_> = rows.iter().map(|row| row.id()).collect();
            // FIXME: we clone all the ids here
            let entity_keys = IdList::try_from_iter(
                group.entity_type.id_type()?,
                entity_keys.into_iter().map(|id| id.to_owned()),
            )?;
            ClampRangeQuery::new(table, &entity_keys, block)?.execute(conn)?;
        }
        section.end();

        let _section = stopwatch.start_section("update_modification_insert_query");
        let mut count = 0;

        // We insert the entities in chunks to make sure each operation does
        // not exceed the maximum number of bindings allowed in queries
        let chunk_size = InsertQuery::chunk_size(table);
        for chunk in group.write_chunks(chunk_size) {
            count += InsertQuery::new(table, &chunk)?.execute(conn)?;
        }

        Ok(count)
    }

    pub fn delete(
        &self,
        conn: &mut PgConnection,
        group: &RowGroup,
        stopwatch: &StopwatchMetrics,
    ) -> Result<usize, StoreError> {
        fn chunk_details(chunk: &IdList) -> String {
            if chunk.len() < 20 {
                let ids = chunk
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("clamp ids [{ids}]")
            } else {
                format!("clamp {} ids", chunk.len())
            }
        }

        if !group.has_clamps() {
            // Nothing to do
            return Ok(0);
        }

        let table = self.table_for_entity(&group.entity_type)?;
        if table.immutable {
            return Err(internal_error!(
                "entities of type `{}` can not be deleted since they are immutable. Entity ids are [{}]",
                table.object, group.ids().join(", ")
            ));
        }

        let _section = stopwatch.start_section("delete_modification_clamp_range_query");
        let mut count = 0;
        for (block, rows) in group.clamps_by_block() {
            let ids: Vec<_> = rows.iter().map(|eref| eref.id()).collect();
            for chunk in ids.chunks(DELETE_OPERATION_CHUNK_SIZE) {
                // FIXME: we clone all the ids here
                let chunk = IdList::try_from_iter(
                    group.entity_type.id_type()?,
                    chunk.into_iter().map(|id| (*id).to_owned()),
                )?;
                count += ClampRangeQuery::new(table, &chunk, block)?
                    .execute(conn)
                    .map_err(|e| {
                        StoreError::write_failure(
                            e,
                            group.entity_type.as_str(),
                            block,
                            chunk_details(&chunk),
                        )
                    })?
            }
        }
        Ok(count)
    }

    pub fn truncate_tables(&self, conn: &mut PgConnection) -> Result<(), StoreError> {
        for table in self.tables.values() {
            sql_query(&format!("TRUNCATE TABLE {}", table.qualified_name)).execute(conn)?;
        }
        Ok(())
    }

    /// Revert the block with number `block` and all blocks with higher
    /// numbers. After this operation, only entity versions inserted or
    /// updated at blocks with numbers strictly lower than `block` will
    /// remain
    ///
    /// The `i32` that is returned is the amount by which the entity count
    /// for the subgraph needs to be adjusted
    pub fn revert_block(
        &self,
        conn: &mut PgConnection,
        block: BlockNumber,
    ) -> Result<i32, StoreError> {
        let mut count: i32 = 0;

        for table in self.tables.values() {
            // Remove all versions whose entire block range lies beyond
            // `block`
            let removed: HashSet<_> = RevertRemoveQuery::new(table, block)
                .get_results::<ReturnedEntityData>(conn)?
                .into_iter()
                .collect();
            // Make the versions current that existed at `block - 1` but that
            // are not current yet. Those are the ones that were updated or
            // deleted at `block`
            let unclamped = if table.immutable {
                HashSet::new()
            } else {
                RevertClampQuery::new(table, block - 1)?
                    .get_results(conn)?
                    .into_iter()
                    .collect::<HashSet<_>>()
            };
            // Adjust the entity count; we can tell which operation was
            // initially performed by
            //   id in (unset - unclamped)  => insert (we now deleted)
            //   id in (unset && unclamped) => update (we reversed the update)
            //   id in (unclamped - unset)  => delete (we now inserted)
            let deleted = removed.difference(&unclamped).count() as i32;
            let inserted = unclamped.difference(&removed).count() as i32;
            count += inserted - deleted;
        }
        Ok(count)
    }

    /// Revert the metadata (dynamic data sources and related entities) for
    /// the given `subgraph`.
    ///
    /// For metadata, reversion always means deletion since the metadata that
    /// is subject to reversion is only ever created but never updated
    pub fn revert_metadata(
        logger: &Logger,
        conn: &mut PgConnection,
        site: &Site,
        block: BlockNumber,
    ) -> Result<(), StoreError> {
        crate::dynds::revert(conn, site, block)?;
        crate::deployment::revert_subgraph_errors(logger, conn, &site.deployment, block)?;

        Ok(())
    }

    pub fn is_cacheable(&self) -> bool {
        // This would be false if we still needed to migrate the Layout, but
        // since there are no migrations in the code right now, it is always
        // safe to cache a Layout
        true
    }

    /// Update the layout with the latest information from the database; an
    /// update can only change the `is_account_like` flag for tables, the
    /// layout's site, or the `history_blocks`. If no update is needed, just
    /// return `self`.
    ///
    /// This is tied closely to how the `LayoutCache` works and called from
    /// it right after creating a `Layout`, and periodically to update the
    /// `Layout` in case changes were made
    fn refresh(
        self: Arc<Self>,
        conn: &mut PgConnection,
        site: Arc<Site>,
    ) -> Result<Arc<Self>, StoreError> {
        let account_like = crate::catalog::account_like(conn, &self.site)?;
        let history_blocks = deployment::history_blocks(conn, &self.site)?;

        let is_account_like = { |table: &Table| account_like.contains(table.name.as_str()) };

        let changed_tables: Vec<_> = self
            .tables
            .values()
            .filter(|table| table.is_account_like != is_account_like(table.as_ref()))
            .collect();
        if changed_tables.is_empty() && site == self.site && history_blocks == self.history_blocks {
            return Ok(self);
        }

        let mut layout = (*self).clone();
        for table in changed_tables.into_iter() {
            let mut table = (*table.as_ref()).clone();
            table.is_account_like = is_account_like(&table);
            layout.tables.insert(table.object.clone(), Arc::new(table));
        }
        layout.site = site;
        layout.history_blocks = history_blocks;
        Ok(Arc::new(layout))
    }

    /// Find the time of the last rollup for the subgraph. We do this by
    /// looking for the maximum timestamp in any aggregation table and
    /// adding a little bit more than the corresponding interval to it. This
    /// method crucially depends on the fact that we always write the rollup
    /// for all aggregations, meaning that if some aggregations do not have
    /// an entry with the maximum timestamp that there was just no data for
    /// that interval, but we did try to aggregate at that time.
    pub(crate) fn last_rollup(
        &self,
        conn: &mut PgConnection,
    ) -> Result<Option<BlockTime>, StoreError> {
        Rollup::last_rollup(&self.rollups, conn)
    }

    /// Construct `Rolllup` for each of the aggregation mappings
    /// `schema.agg_mappings()` and return them in the same order as the
    /// aggregation mappings
    fn rollups(
        tables: &HashMap<EntityType, Arc<Table>>,
        schema: &InputSchema,
    ) -> Result<Vec<Rollup>, StoreError> {
        let mut rollups = Vec::new();
        for mapping in schema.agg_mappings() {
            let source_type = mapping.source_type(schema);
            let source_table = tables
                .get(&source_type)
                .ok_or_else(|| internal_error!("Table for {source_type} is missing"))?;
            let agg_type = mapping.agg_type(schema);
            let agg_table = tables
                .get(&agg_type)
                .ok_or_else(|| internal_error!("Table for {agg_type} is missing"))?;
            let aggregation = mapping.aggregation(schema);
            let rollup = Rollup::new(
                mapping.interval,
                aggregation,
                source_table,
                agg_table.cheap_clone(),
            )?;
            rollups.push(rollup);
        }
        Ok(rollups)
    }

    /// Roll up all timeseries for each entry in `block_times`. The overall
    /// effect is that all buckets that end after `last_rollup` and before
    /// the last entry in `block_times` are filled. This will fill all
    /// buckets whose end time `end` is in `last_rollup < end <=
    /// block_time`. The rollups happen stepwise, for each entry in
    /// `block_times` so that the buckets are associated with the block
    /// number for those block times.
    ///
    /// We roll up all pending aggregations and mark them as belonging to
    /// the block where the timestamp first fell into a new time period. We
    /// only know about blocks where the subgraph actually performs a write.
    /// That means that the block is not necessarily the block at the end of
    /// the time period but might be a block after that time period if the
    /// subgraph skips blocks. This can be a problem for time-travel queries
    /// by block as it might not find a rollup that had occurred but is
    /// marked with a later block; this is not an issue when writes happen
    /// at every block. Queries for aggregations should therefore not do
    /// time-travel by block number but rather by timestamp.
    ///
    /// It can also lead to unnecessarily reverting a rollup, but in that
    /// case the results will be correct, we just do work that might not
    /// have been necessary had we marked the rollup with the precise
    /// smaller block number instead of the one we are using here.
    ///
    /// Changing this would require that we have a complete list of block
    /// numbers and block times which we do not have anywhere in graph-node.
    pub(crate) fn rollup(
        &self,
        conn: &mut PgConnection,
        last_rollup: Option<BlockTime>,
        block_times: &[(BlockNumber, BlockTime)],
    ) -> Result<(), StoreError> {
        if block_times.is_empty() {
            return Ok(());
        }

        // If we have never done a rollup, we can just use the smallest
        // block time we are getting as the time for the last rollup
        let mut last_rollup = last_rollup.unwrap_or_else(|| {
            block_times
                .iter()
                .map(|(_, block_time)| *block_time)
                .min()
                .unwrap()
        });
        // The for loop could be eliminated if the rollup queries could deal
        // with the full `block_times` vector, but the SQL for that will be
        // very complicated and is left for a future improvement.
        for (block, block_time) in block_times {
            for rollup in &self.rollups {
                let buckets = rollup.interval.buckets(last_rollup, *block_time);
                // We only need to pay attention to the first bucket; if
                // there are more buckets, there's nothing to rollup for
                // them as the next changes we wrote are for `block_time`,
                // and we'll catch that on the next iteration of the loop.
                //
                // Assume we are passed `block_times = [b1, b2, b3, .. ]`
                // but b1 and b2 are far apart. We call
                // `rollup.interval.buckets(b1, b2)` at some point in the
                // iteration which produces timestamps `[t1, t2, ..]`. Since
                // b1 and b2 are far apart, we have something like `t1 <= b1
                // < t2 < t3 < t4 < t5 <= b2` but we know that there are no
                // writes between `b1` and `b2` - if there were, we'd have
                // some block time in `block_times` between `b1` and `b2`.
                // So we only need to do a rollup for `t1 < b1 < t2`. After
                // that, we set `last_rollup = b2` and repeat the loop for
                // that, which will roll up the bucket `t5 <= b2 < t6`. So
                // there's no need to worry about the buckets starting at
                // `t2`, `t3`, and `t4`.
                match buckets.first() {
                    None => {
                        // The rollups are in increasing order of interval size, so
                        // if a smaller interval doesn't have a bucket between
                        // last_rollup and block_time, a larger one can't either and
                        // we are done with this rollup.
                        break;
                    }
                    Some(bucket) => {
                        rollup.insert(conn, &bucket, *block)?;
                    }
                }
            }
            last_rollup = *block_time;
        }
        Ok(())
    }
}

/// A user-defined enum
#[derive(Clone, Debug, PartialEq)]
pub struct EnumType {
    /// The name of the Postgres enum we created, fully qualified with the schema
    pub name: SqlName,
    /// The possible values the enum can take
    values: Arc<BTreeSet<String>>,
}

impl EnumType {
    fn is_assignable_from(&self, source: &Self) -> Option<String> {
        if source.values.is_subset(self.values.as_ref()) {
            None
        } else {
            Some(format!(
                "the enum type {} contains values not present in {}",
                source.name, self.name
            ))
        }
    }
}

/// This is almost the same as graph::data::store::ValueType, but without
/// ID and List; with this type, we only care about scalar types that directly
/// correspond to Postgres scalar types
#[derive(Clone, Debug, PartialEq)]
pub enum ColumnType {
    Boolean,
    BigDecimal,
    BigInt,
    Bytes,
    Int,
    Int8,
    Timestamp,
    String,
    TSVector(FulltextConfig),
    Enum(EnumType),
}

impl From<IdType> for ColumnType {
    fn from(id_type: IdType) -> Self {
        match id_type {
            IdType::Bytes => ColumnType::Bytes,
            IdType::String => ColumnType::String,
            IdType::Int8 => ColumnType::Int8,
        }
    }
}

impl std::fmt::Display for ColumnType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ColumnType::Boolean => write!(f, "Boolean"),
            ColumnType::BigDecimal => write!(f, "BigDecimal"),
            ColumnType::BigInt => write!(f, "BigInt"),
            ColumnType::Bytes => write!(f, "Bytes"),
            ColumnType::Int => write!(f, "Int"),
            ColumnType::Int8 => write!(f, "Int8"),
            ColumnType::Timestamp => write!(f, "Timestamp"),
            ColumnType::String => write!(f, "String"),
            ColumnType::TSVector(_) => write!(f, "TSVector"),
            ColumnType::Enum(enum_type) => write!(f, "Enum({})", enum_type.name),
        }
    }
}

impl ColumnType {
    fn from_field_type(
        schema: &InputSchema,
        field_type: &q::Type,
        catalog: &Catalog,
        is_existing_text_column: bool,
    ) -> Result<ColumnType, StoreError> {
        let name = field_type.get_base_type();

        // See if its an object type defined in the schema
        if let Some(id_type) = schema
            .entity_type(name)
            .ok()
            .and_then(|entity_type| Some(entity_type.id_type()))
            .transpose()?
        {
            return Ok(id_type.into());
        }

        // Check if it's an enum, and if it is, return an appropriate
        // ColumnType::Enum
        if let Some(values) = schema.enum_values(name) {
            // We do things this convoluted way to make sure field_type gets
            // snakecased, but the `.` must stay a `.`
            let name = SqlName::qualified_name(&catalog.site.namespace, &SqlName::from(name));
            if is_existing_text_column {
                // We used to have a bug where columns that should have really
                // been of an enum type were created as text columns. To make
                // queries work against such misgenerated tables, we pretend
                // that this GraphQL attribute is really of type `String`
                return Ok(ColumnType::String);
            } else {
                return Ok(ColumnType::Enum(EnumType {
                    name,
                    values: values.clone(),
                }));
            }
        }

        // It is not an object type or an enum, and therefore one of our
        // builtin primitive types
        match ValueType::from_str(name)? {
            ValueType::Boolean => Ok(ColumnType::Boolean),
            ValueType::BigDecimal => Ok(ColumnType::BigDecimal),
            ValueType::BigInt => Ok(ColumnType::BigInt),
            ValueType::Bytes => Ok(ColumnType::Bytes),
            ValueType::Int => Ok(ColumnType::Int),
            ValueType::Int8 => Ok(ColumnType::Int8),
            ValueType::Timestamp => Ok(ColumnType::Timestamp),
            ValueType::String => Ok(ColumnType::String),
        }
    }

    pub fn sql_type(&self) -> &str {
        match self {
            ColumnType::Boolean => "boolean",
            ColumnType::BigDecimal => "numeric",
            ColumnType::BigInt => "numeric",
            ColumnType::Bytes => "bytea",
            ColumnType::Int => "int4",
            ColumnType::Int8 => "int8",
            ColumnType::Timestamp => "timestamptz",
            ColumnType::String => "text",
            ColumnType::TSVector(_) => "tsvector",
            ColumnType::Enum(enum_type) => enum_type.name.as_str(),
        }
    }

    /// Return the `IdType` corresponding to this column type. This can only
    /// be called on a column that stores an `ID` and will return an error
    pub(crate) fn id_type(&self) -> QueryResult<IdType> {
        match self {
            ColumnType::String => Ok(IdType::String),
            ColumnType::Bytes => Ok(IdType::Bytes),
            ColumnType::Int8 => Ok(IdType::Int8),
            _ => Err(diesel::result::Error::QueryBuilderError(
                anyhow!(
                    "only String, Bytes, and Int8 are allowed as primary keys but not {:?}",
                    self
                )
                .into(),
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Column {
    pub name: SqlName,
    pub field: Word,
    pub field_type: q::Type,
    pub column_type: ColumnType,
    pub fulltext_fields: Option<HashSet<String>>,
    is_reference: bool,
    /// Whether to use a prefix of the column for comparisons and index
    /// creation, or column values in their entirety
    pub use_prefix_comparison: bool,
}

impl Column {
    fn new(
        schema: &InputSchema,
        table_name: &SqlName,
        field: &Field,
        catalog: &Catalog,
    ) -> Result<Column, StoreError> {
        SqlName::check_valid_identifier(&field.name, "attribute")?;

        let sql_name = SqlName::from(&*field.name);

        let is_reference = schema.is_reference(&field.field_type.get_base_type());

        let column_type = if sql_name.as_str() == PRIMARY_KEY_COLUMN {
            IdType::try_from(&field.field_type)?.into()
        } else {
            let is_existing_text_column = catalog.is_existing_text_column(table_name, &sql_name);
            ColumnType::from_field_type(
                schema,
                &field.field_type,
                catalog,
                is_existing_text_column,
            )?
        };
        let is_primary_key = sql_name.as_str() == PRIMARY_KEY_COLUMN;

        // When a column has arbitrary size, we only index a prefix of the
        // column to avoid errors caused by inserting values that are too
        // large for the index.
        //
        // Since we already have installations where `Bytes` columns had
        // been indexed in their entirety, we remember if a specific
        // subgraph indexes that, or just a prefix of `Bytes` columns. Query
        // generation needs to match how these columns are indexed, and we
        // therefore use that remembered value from `catalog` to determine
        // if we should use queries for prefixes or for the entire value.
        // see: attr-bytea-prefix
        let use_prefix_comparison = !is_primary_key
            && !is_reference
            && !field.field_type.is_list()
            && (column_type == ColumnType::String
                || (column_type == ColumnType::Bytes && catalog.use_bytea_prefix));

        Ok(Column {
            name: sql_name,
            field: field.name.clone(),
            column_type,
            field_type: field.field_type.clone(),
            fulltext_fields: None,
            is_reference,
            use_prefix_comparison,
        })
    }

    pub fn pseudo_column(name: &str, column_type: ColumnType) -> Column {
        let field_type = q::Type::NamedType(column_type.to_string());
        let name = SqlName::verbatim(name.to_string());
        let field = Word::from(name.as_str());
        Column {
            name,
            field,
            field_type,
            column_type,
            fulltext_fields: None,
            is_reference: false,
            use_prefix_comparison: false,
        }
    }

    fn new_fulltext(def: &FulltextDefinition) -> Result<Column, StoreError> {
        SqlName::check_valid_identifier(&def.name, "attribute")?;
        let sql_name = SqlName::from(def.name.as_str());

        Ok(Column {
            name: sql_name,
            field: Word::from(def.name.to_string()),
            field_type: q::Type::NamedType("fulltext".to_string()),
            column_type: ColumnType::TSVector(def.config.clone()),
            fulltext_fields: Some(def.included_fields.clone()),
            is_reference: false,
            use_prefix_comparison: false,
        })
    }

    fn sql_type(&self) -> &str {
        self.column_type.sql_type()
    }

    pub fn is_nullable(&self) -> bool {
        fn is_nullable(field_type: &q::Type) -> bool {
            match field_type {
                q::Type::NonNullType(_) => false,
                _ => true,
            }
        }
        is_nullable(&self.field_type)
    }

    pub fn is_list(&self) -> bool {
        self.field_type.is_list()
    }

    pub fn is_enum(&self) -> bool {
        matches!(self.column_type, ColumnType::Enum(_))
    }

    pub fn is_fulltext(&self) -> bool {
        self.field_type.get_base_type() == "fulltext"
    }

    pub fn is_reference(&self) -> bool {
        self.is_reference
    }

    pub fn is_primary_key(&self) -> bool {
        self.name.as_str() == PRIMARY_KEY_COLUMN
    }

    pub fn is_assignable_from(&self, source: &Self, object: &EntityType) -> Option<String> {
        if !self.is_nullable() && source.is_nullable() {
            Some(format!(
                "The attribute {}.{} is non-nullable, \
                             but the corresponding attribute in the source is nullable",
                object, self.field
            ))
        } else if let ColumnType::Enum(self_enum_type) = &self.column_type {
            if let ColumnType::Enum(source_enum_type) = &source.column_type {
                self_enum_type.is_assignable_from(source_enum_type)
            } else {
                Some(format!(
                    "The attribute {}.{} is an enum {}, \
                                 but its type in the source is {}",
                    object, self.field, self.field_type, source.field_type
                ))
            }
        } else if self.column_type != source.column_type || self.is_list() != source.is_list() {
            Some(format!(
                "The attribute {}.{} has type {}, \
                             but its type in the source is {}",
                object, self.field, self.field_type, source.field_type
            ))
        } else {
            None
        }
    }
}

/// The name for the primary key column of a table; hardcoded for now
pub(crate) const PRIMARY_KEY_COLUMN: &str = "id";

/// We give every version of every entity in our tables, i.e., every row, a
/// synthetic primary key. This is the name of the column we use.
pub(crate) const VID_COLUMN: &str = "vid";

#[derive(Debug, Clone)]
pub struct Table {
    /// The reference to the underlying type in the input schema. For
    /// aggregations, this is the object type for a specific interval, like
    /// `Stats_hour`, not the overall aggregation type `Stats`.
    pub object: EntityType,

    /// The namespace in which the table lives
    nsp: Namespace,

    /// The name of the database table for this type ('thing'), snakecased
    /// version of `object`
    pub name: SqlName,

    /// The table name qualified with the schema in which the table lives,
    /// `schema.table`
    pub qualified_name: SqlName,

    pub columns: Vec<Column>,

    /// This kind of entity behaves like an account in that it has a low
    /// ratio of distinct entities to overall number of rows because
    /// entities are updated frequently on average
    pub is_account_like: bool,

    /// The position of this table in all the tables for this layout; this
    /// is really only needed for the tests to make the names of indexes
    /// predictable
    position: u32,

    /// Entities in this table are immutable, i.e., will never be updated or
    /// deleted
    pub(crate) immutable: bool,

    /// Whether this table has an explicit `causality_region` column. If `false`, then the column is
    /// not present and the causality region for all rows is implicitly `0` (equivalent to CasualityRegion::ONCHAIN).
    pub(crate) has_causality_region: bool,
}

impl Table {
    fn new(
        schema: &InputSchema,
        defn: &EntityType,
        catalog: &Catalog,
        fulltexts: Vec<FulltextDefinition>,
        position: u32,
        has_causality_region: bool,
    ) -> Result<Table, StoreError> {
        SqlName::check_valid_identifier(defn.as_str(), "object")?;

        let object_type = defn
            .object_type()
            .map_err(|_| internal_error!("The type `{}` is not an object type", defn.as_str()))?;

        let table_name = SqlName::from(defn.as_str());
        let columns = object_type
            .fields
            .iter()
            .filter(|field| !field.is_derived())
            .map(|field| Column::new(schema, &table_name, field, catalog))
            .chain(fulltexts.iter().map(Column::new_fulltext))
            .collect::<Result<Vec<Column>, StoreError>>()?;
        let qualified_name = SqlName::qualified_name(&catalog.site.namespace, &table_name);
        let immutable = defn.is_immutable();
        let nsp = catalog.site.namespace.clone();
        let table = Table {
            object: defn.cheap_clone(),
            name: table_name,
            nsp,
            qualified_name,
            // Default `is_account_like` to `false`; the caller should call
            // `refresh` after constructing the layout, but that requires a
            // db connection, which we don't have at this point.
            is_account_like: false,
            columns,
            position,
            immutable,
            has_causality_region,
        };
        Ok(table)
    }

    /// Create a table that is like `self` except that its name in the
    /// database is based on `namespace` and `name`
    pub fn new_like(&self, namespace: &Namespace, name: &SqlName) -> Arc<Table> {
        let other = Table {
            object: self.object.clone(),
            nsp: namespace.clone(),
            name: name.clone(),
            qualified_name: SqlName::qualified_name(namespace, name),
            columns: self.columns.clone(),
            is_account_like: self.is_account_like,
            position: self.position,
            immutable: self.immutable,
            has_causality_region: self.has_causality_region,
        };

        Arc::new(other)
    }

    /// Find the column `name` in this table. The name must be in snake case,
    /// i.e., use SQL conventions
    pub fn column(&self, name: &SqlName) -> Option<&Column> {
        self.columns
            .iter()
            .filter(|column| match column.column_type {
                ColumnType::TSVector(_) => false,
                _ => true,
            })
            .find(|column| &column.name == name)
    }

    /// Find the column for `field` in this table. The name must be the
    /// GraphQL name of an entity field
    pub fn column_for_field(&self, field: &str) -> Result<&Column, StoreError> {
        self.columns
            .iter()
            .find(|column| column.field == field)
            .ok_or_else(|| StoreError::UnknownField(self.name.to_string(), field.to_string()))
    }

    fn can_copy_from(&self, source: &Self) -> Vec<String> {
        self.columns
            .iter()
            .filter_map(|dcol| match source.column(&dcol.name) {
                Some(scol) => dcol.is_assignable_from(scol, &self.object),
                None => {
                    if !dcol.is_nullable() {
                        Some(format!(
                            "The attribute {}.{} is non-nullable, \
                         but there is no such attribute in the source",
                            self.object, dcol.field
                        ))
                    } else {
                        None
                    }
                }
            })
            .collect()
    }

    pub fn primary_key(&self) -> &Column {
        self.columns
            .iter()
            .find(|column| column.is_primary_key())
            .expect("every table has a primary key")
    }

    pub(crate) fn analyze(&self, conn: &mut PgConnection) -> Result<(), StoreError> {
        let table_name = &self.qualified_name;
        let sql = format!("analyze (skip_locked) {table_name}");
        sql_query(&sql).execute(conn)?;
        Ok(())
    }

    pub(crate) fn block_column(&self) -> &SqlName {
        if self.immutable {
            &crate::block_range::BLOCK_COLUMN_SQL
        } else {
            &crate::block_range::BLOCK_RANGE_COLUMN_SQL
        }
    }

    pub fn dsl_table(&self) -> dsl::Table<'_> {
        dsl::Table::new(self)
    }
}

#[derive(Clone)]
struct CacheEntry {
    value: Arc<Layout>,
    expires: Instant,
}

/// Cache layouts for some time and refresh them when they expire.
/// Refreshing happens one at a time, and the cache makes sure we minimize
/// blocking while a refresh happens, favoring using an expired layout over
/// a refreshed one.
pub struct LayoutCache {
    entries: Mutex<HashMap<DeploymentHash, CacheEntry>>,
    ttl: Duration,
    /// Use this so that we only refresh one layout at any given time to
    /// avoid refreshing the same layout multiple times
    refresh: Mutex<()>,
    last_sweep: Mutex<Instant>,
}

impl LayoutCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
            refresh: Mutex::new(()),
            last_sweep: Mutex::new(Instant::now()),
        }
    }

    fn load(conn: &mut PgConnection, site: Arc<Site>) -> Result<Arc<Layout>, StoreError> {
        let (subgraph_schema, use_bytea_prefix) = deployment::schema(conn, site.as_ref())?;
        let has_causality_region =
            deployment::entities_with_causality_region(conn, site.id, &subgraph_schema)?;
        let catalog = Catalog::load(conn, site.clone(), use_bytea_prefix, has_causality_region)?;
        let layout = Arc::new(Layout::new(site.clone(), &subgraph_schema, catalog)?);
        layout.refresh(conn, site)
    }

    fn cache(&self, layout: Arc<Layout>) {
        if self.ttl > Duration::ZERO && layout.is_cacheable() {
            let deployment = layout.site.deployment.clone();
            let entry = CacheEntry {
                expires: Instant::now() + self.ttl,
                value: layout,
            };
            self.entries.lock().unwrap().insert(deployment, entry);
        }
    }

    /// Return the corresponding layout if we have one in cache already, and
    /// ignore expiration information
    pub(crate) fn find(&self, site: &Site) -> Option<Arc<Layout>> {
        self.entries
            .lock()
            .unwrap()
            .get(&site.deployment)
            .map(|CacheEntry { value, expires: _ }| value.clone())
    }

    /// Get the layout for `site`. If it's not in cache, load it. If it is
    /// expired, try to refresh it if there isn't another refresh happening
    /// already
    pub fn get(
        &self,
        logger: &Logger,
        conn: &mut PgConnection,
        site: Arc<Site>,
    ) -> Result<Arc<Layout>, StoreError> {
        let now = Instant::now();
        let entry = {
            let lock = self.entries.lock().unwrap();
            lock.get(&site.deployment).cloned()
        };
        let layout = match entry {
            Some(CacheEntry { value, expires }) => {
                if now <= expires {
                    // Entry is not expired; use it
                    value
                } else {
                    // Only do a cache refresh once; we don't want to have
                    // multiple threads refreshing the same layout
                    // simultaneously. It's easiest to refresh at most one
                    // layout globally
                    let refresh = self.refresh.try_lock();
                    if refresh.is_err() {
                        value
                    } else {
                        self.refresh(logger, conn, site, value)
                    }
                }
            }
            None => {
                let layout = Self::load(conn, site)?;
                self.cache(layout.cheap_clone());
                layout
            }
        };
        self.sweep(now);
        Ok(layout)
    }

    fn refresh(
        &self,
        logger: &Logger,
        conn: &mut PgConnection,
        site: Arc<Site>,
        value: Arc<Layout>,
    ) -> Arc<Layout> {
        match value.cheap_clone().refresh(conn, site) {
            Err(e) => {
                warn!(
                    logger,
                    "failed to refresh statistics. Continuing with old statistics";
                    "deployment" => &value.site.deployment,
                    "error" => e.to_string()
                );
                // Update the timestamp so we don't retry
                // refreshing too often
                self.cache(value.cheap_clone());
                value
            }
            Ok(layout) => {
                self.cache(layout.cheap_clone());
                layout
            }
        }
    }

    pub(crate) fn remove(&self, site: &Site) -> Option<Arc<Layout>> {
        self.entries
            .lock()
            .unwrap()
            .remove(&site.deployment)
            .map(|CacheEntry { value, expires: _ }| value)
    }

    // Only needed for tests
    #[cfg(debug_assertions)]
    pub(crate) fn clear(&self) {
        self.entries.lock().unwrap().clear()
    }

    /// Periodically sweep the cache to remove expired entries; an entry is
    /// expired if it was last updated more than 2*self.ttl ago
    fn sweep(&self, now: Instant) {
        if now - *self.last_sweep.lock().unwrap() < ENV_VARS.store.schema_cache_ttl {
            return;
        }
        let mut entries = self.entries.lock().unwrap();
        // We allow entries to stick around for 2*ttl; if an entry was used
        // in that time, it will get refreshed and have its expiry updated
        entries.retain(|_, entry| entry.expires + self.ttl > now);
        *self.last_sweep.lock().unwrap() = now;
    }
}
