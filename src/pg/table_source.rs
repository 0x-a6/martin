use crate::pg::config::{PgInfo, TableInfo};
use crate::pg::configurator::SqlTableInfoMapMapMap;
use crate::pg::pg_source::PgSqlInfo;
use crate::pg::pool::Pool;
use crate::pg::utils::{io_error, json_to_hashmap, polygon_to_bbox};
use crate::utils::normalize_key;
use log::{info, warn};
use postgres_protocol::escape::{escape_identifier, escape_literal};
use std::collections::HashMap;
use std::io;

static DEFAULT_EXTENT: u32 = 4096;
static DEFAULT_BUFFER: u32 = 64;
static DEFAULT_CLIP_GEOM: bool = true;

#[derive(Clone, Debug)]
pub struct PgSqlTableInfo {
    pub info: TableInfo,
}

pub async fn get_table_sources(pool: &Pool) -> Result<SqlTableInfoMapMapMap, io::Error> {
    let conn = pool.get().await?;
    let rows = conn
        .query(include_str!("scripts/get_table_sources.sql"), &[])
        .await
        .map_err(|e| io_error!(e, "Can't get table sources"))?;

    let mut res = SqlTableInfoMapMapMap::new();
    for row in &rows {
        let info = TableInfo {
            schema: row.get("schema"),
            table: row.get("name"),
            geometry_column: row.get("geom"),
            srid: row.get("srid"), // casting i32 to u32?
            extent: Some(DEFAULT_EXTENT),
            buffer: Some(DEFAULT_BUFFER),
            clip_geom: Some(DEFAULT_CLIP_GEOM),
            geometry_type: row.get("type"),
            properties: json_to_hashmap(&row.get("properties")),
            unrecognized: HashMap::new(),
            ..TableInfo::default()
        };

        if let Some(v) = res
            .entry(info.schema.clone())
            .or_insert_with(HashMap::new)
            .entry(info.table.clone())
            .or_insert_with(HashMap::new)
            .insert(info.geometry_column.clone(), info)
        {
            warn!("Unexpected duplicate table {}", v.format_id());
        }
    }

    Ok(res)
}

fn escape_with_alias(mapping: &HashMap<String, String>, field: &str) -> String {
    let column = mapping.get(field).map_or(field, |v| v.as_str());
    if field != column {
        format!(
            ", {} AS {}",
            escape_identifier(column),
            escape_identifier(field),
        )
    } else {
        format!(", {}", escape_identifier(column))
    }
}

pub async fn table_to_query(
    id: String,
    mut info: TableInfo,
    pool: Pool,
) -> Result<(String, PgSqlInfo, TableInfo), io::Error> {
    let bounds_query = format!(
        include_str!("scripts/get_bounds.sql"),
        schema = info.schema,
        table = info.table,
        srid = info.srid,
        geometry_column = info.geometry_column,
    );

    if info.bounds.is_none() {
        info.bounds = pool
            .get()
            .await?
            .query_one(bounds_query.as_str(), &[])
            .await
            .map(|row| row.get("bounds"))
            .ok()
            .flatten()
            .and_then(|v| polygon_to_bbox(&v));
    }

    let properties = if info.properties.is_empty() {
        String::new()
    } else {
        info.properties
            .keys()
            .map(|column| escape_with_alias(&info.prop_mapping, column))
            .collect::<String>()
    };

    let (id_name, id_field) = if let Some(id_column) = &info.id_column {
        (
            format!(", {}", escape_literal(id_column)),
            escape_with_alias(&info.prop_mapping, id_column),
        )
    } else {
        (String::new(), String::new())
    };

    let extent = info.extent.unwrap_or(DEFAULT_EXTENT);
    let buffer = info.buffer.unwrap_or(DEFAULT_BUFFER);

    let bbox_search = if buffer == 0 {
        "ST_TileEnvelope($1::integer, $2::integer, $3::integer)".to_string()
    } else if pool.supports_tile_margin() {
        let margin = buffer as f64 / extent as f64;
        format!("ST_TileEnvelope($1::integer, $2::integer, $3::integer, margin => {margin})")
    } else {
        // TODO: we should use ST_Expand here, but it may require a bit more math work,
        //       so might not be worth it as it is only used for PostGIS < v3.1.
        //       v3.1 has been out for 2+ years (december 2020)
        // let earth_circumference = 40075016.6855785;
        // let val = earth_circumference * buffer as f64 / extent as f64;
        // format!("ST_Expand(ST_TileEnvelope($1::integer, $2::integer, $3::integer), {val}/2^$1::integer)")
        "ST_TileEnvelope($1::integer, $2::integer, $3::integer)".to_string()
    };

    let query = format!(
        r#"
SELECT
  ST_AsMVT(tile, {table_id}, {extent}, 'geom'{id_name})
FROM (
  SELECT
    ST_AsMVTGeom(
        ST_Transform(ST_CurveToLine({geometry_column}), 3857),
        ST_TileEnvelope($1::integer, $2::integer, $3::integer),
        {extent}, {buffer}, {clip_geom}
    ) AS geom
    {id_field}{properties}
  FROM
    {schema}.{table}
  WHERE
    {geometry_column} && ST_Transform({bbox_search}, {srid})
) AS tile
"#,
        table_id = escape_literal(info.format_id().as_str()),
        geometry_column = escape_identifier(&info.geometry_column),
        clip_geom = info.clip_geom.unwrap_or(DEFAULT_CLIP_GEOM),
        schema = escape_identifier(&info.schema),
        table = escape_identifier(&info.table),
        srid = info.srid,
    )
    .trim()
    .to_string();

    Ok((id, PgSqlInfo::new(query, false, info.format_id()), info))
}

pub fn merge_table_info(
    default_srid: Option<i32>,
    new_id: &String,
    cfg_inf: &TableInfo,
    src_inf: &TableInfo,
) -> Option<TableInfo> {
    // Assume cfg_inf and src_inf have the same schema/table/geometry_column
    let table_id = src_inf.format_id();
    let mut inf = TableInfo {
        // These values must match the database exactly
        schema: src_inf.schema.clone(),
        table: src_inf.table.clone(),
        geometry_column: src_inf.geometry_column.clone(),
        srid: calc_srid(&table_id, new_id, src_inf.srid, cfg_inf.srid, default_srid)?,
        prop_mapping: HashMap::new(),
        ..cfg_inf.clone()
    };

    match (&src_inf.geometry_type, &cfg_inf.geometry_type) {
        (Some(src), Some(cfg)) if src != cfg => {
            warn!(r#"Table {table_id} has geometry type={src}, but source {new_id} has {cfg}"#);
        }
        _ => {}
    }

    if let Some(id_column) = &cfg_inf.id_column {
        let prop = normalize_key(&src_inf.properties, id_column.as_str(), "id_column", new_id)?;
        inf.prop_mapping.insert(id_column.clone(), prop);
    }

    for key in cfg_inf.properties.keys() {
        let prop = normalize_key(&src_inf.properties, key.as_str(), "property", new_id)?;
        inf.prop_mapping.insert(key.clone(), prop);
    }

    Some(inf)
}

pub fn calc_srid(
    table_id: &str,
    new_id: &str,
    src_srid: i32,
    cfg_srid: i32,
    default_srid: Option<i32>,
) -> Option<i32> {
    match (src_srid, cfg_srid, default_srid) {
        (0, 0, Some(default_srid)) => {
            info!("Table {table_id} has SRID=0, using provided default SRID={default_srid}");
            Some(default_srid)
        }
        (0, 0, None) => {
            let info = "To use this table source, set default or specify this table SRID in the config file, or set the default SRID with  --default-srid=...";
            warn!("Table {table_id} has SRID=0, skipping. {info}");
            None
        }
        (0, cfg, _) => Some(cfg), // Use the configured SRID
        (src, 0, _) => Some(src), // Use the source SRID
        (src, cfg, _) if src != cfg => {
            warn!("Table {table_id} has SRID={src}, but source {new_id} has SRID={cfg}");
            None
        }
        (_, cfg, _) => Some(cfg),
    }
}