//! Persistent host-only markers for protected notice sinks and grouped publication.
use excalibur_store::{
    Database, Priority, Result,
    rusqlite::{Connection, OptionalExtension},
};
pub(super) async fn migrate(db: &Database) -> crate::Result<()> {
    db.write(Priority::Journal, |c| {
        use sha2::Digest;
        let schema = include_str!("schema_v6.sql");
        let checksum = format!("{:x}", sha2::Sha256::digest(schema.as_bytes()));
        let known: Option<String> = c
            .query_row(
                "SELECT checksum FROM master_schema WHERE version=6",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if known.as_ref().is_some_and(|old| old != &checksum) {
            return Err(excalibur_store::Error::Invalid(
                "master schema checksum mismatch",
            ));
        }
        c.execute_batch(schema)?;
        c.execute(
            "INSERT OR IGNORE INTO master_schema(version,checksum) VALUES(6,?1)",
            [checksum],
        )?;
        Ok(())
    })
    .await?;
    Ok(())
}
pub(super) fn origin(c: &Connection, id: &str) -> Result<Option<String>> {
    Ok(c.query_row(
        "SELECT notification_id FROM master_notice_guards WHERE notice_id=?1",
        [id],
        |r| r.get(0),
    )
    .optional()?)
}
pub(super) fn bind(c: &Connection, id: &str, notification: &str) -> Result<()> {
    if origin(c, id)?.is_some_and(|old| old != notification) {
        return Err(excalibur_store::Error::Invalid("host notice origin reused"));
    }
    c.execute(
        "INSERT OR IGNORE INTO master_notice_guards(notice_id,notification_id) VALUES(?1,?2)",
        (id, notification),
    )?;
    Ok(())
}
