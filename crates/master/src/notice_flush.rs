//! Durable rotation bounds each writer call to 128 groups and 32 members/group.
use excalibur_store::{
    Database, Priority, Result,
    rusqlite::{Connection, OptionalExtension},
};
pub(super) struct Group {
    pub conversation: String,
    pub scope: String,
    pub due: i64,
}
pub(super) async fn migrate(db: &Database) -> crate::Result<()> {
    db.write(Priority::Journal, |c| {
        use sha2::Digest;
        let schema = include_str!("schema_v7.sql");
        let checksum = format!("{:x}", sha2::Sha256::digest(schema.as_bytes()));
        let known: Option<String> = c
            .query_row(
                "SELECT checksum FROM master_schema WHERE version=7",
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
            "INSERT OR IGNORE INTO master_schema(version,checksum) VALUES(7,?1)",
            [checksum],
        )?;
        Ok(())
    })
    .await?;
    Ok(())
}
pub(super) fn groups(c: &Connection, principal: &str, now: i64) -> Result<Vec<Group>> {
    let cursor:Option<(Group,Group)>=c.query_row("SELECT due_at,conversation_id,scope_id,round_due_at,round_conversation_id,round_scope_id FROM master_notice_group_cursors WHERE principal_id=?1",[principal],|r|Ok((Group{due:r.get(0)?,conversation:r.get(1)?,scope:r.get(2)?},Group{due:r.get(3)?,conversation:r.get(4)?,scope:r.get(5)?}))).optional()?;
    let ceiling = || -> Result<Option<Group>> {
        Ok(c.query_row("SELECT o.due_at,o.conversation_id,o.scope_id FROM master_notice_outbox o JOIN conversations v ON v.id=o.conversation_id WHERE v.principal_id=?1 AND o.flushed=0 AND o.due_at<=?2 ORDER BY o.due_at DESC,o.conversation_id DESC,o.scope_id DESC LIMIT 1",(principal,now),|r|Ok(Group{due:r.get(0)?,conversation:r.get(1)?,scope:r.get(2)?})).optional()?)
    };
    let page = |after: Option<&Group>, cap: &Group| -> Result<Vec<Group>> {
        let (due, conversation, scope) = match after {
            None => (None, None, None),
            Some(g) => (
                Some(g.due),
                Some(g.conversation.as_str()),
                Some(g.scope.as_str()),
            ),
        };
        let mut query=c.prepare("SELECT o.conversation_id,o.scope_id,o.due_at FROM master_notice_outbox o JOIN conversations v ON v.id=o.conversation_id WHERE v.principal_id=?1 AND o.flushed=0 AND o.due_at<=?2 AND (?3 IS NULL OR (o.due_at,o.conversation_id,o.scope_id)>(?3,?4,?5)) AND (o.due_at,o.conversation_id,o.scope_id)<=(?6,?7,?8) GROUP BY o.due_at,o.conversation_id,o.scope_id ORDER BY o.due_at,o.conversation_id,o.scope_id LIMIT 128")?;
        Ok(query
            .query_map(
                (
                    principal,
                    now,
                    due,
                    conversation,
                    scope,
                    cap.due,
                    &cap.conversation,
                    &cap.scope,
                ),
                |r| {
                    Ok(Group {
                        conversation: r.get(0)?,
                        scope: r.get(1)?,
                        due: r.get(2)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    };
    let (mut groups, mut cap) = match cursor.as_ref() {
        Some((after, cap)) => (page(Some(after), cap)?, Some(cap)),
        None => (Vec::new(), None),
    };
    let fresh_cap;
    if groups.is_empty() {
        fresh_cap = ceiling()?;
        cap = fresh_cap.as_ref();
        if let Some(cap) = cap {
            groups = page(None, cap)?;
        }
    }
    if let (Some(last), Some(cap)) = (groups.last(), cap) {
        c.execute("INSERT INTO master_notice_group_cursors VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(principal_id) DO UPDATE SET due_at=excluded.due_at,conversation_id=excluded.conversation_id,scope_id=excluded.scope_id,round_due_at=excluded.round_due_at,round_conversation_id=excluded.round_conversation_id,round_scope_id=excluded.round_scope_id",(principal,last.due,&last.conversation,&last.scope,cap.due,&cap.conversation,&cap.scope))?;
    } else {
        c.execute(
            "DELETE FROM master_notice_group_cursors WHERE principal_id=?1",
            [principal],
        )?;
    }
    // A fixed round ceiling prevents continuously arriving later groups from
    // keeping new early groups behind the cursor forever. At most two bounded
    // page queries run, and at most 128 groups are returned in one writer call.
    Ok(groups)
}
pub(super) fn members(
    c: &Connection,
    principal: &str,
    group: &Group,
) -> Result<Vec<(String, String)>> {
    let cursor:Option<(String,String)>=c.query_row("SELECT notice_id,round_notice_id FROM master_notice_member_cursors WHERE principal_id=?1 AND conversation_id=?2 AND scope_id=?3 AND due_at=?4",(principal,&group.conversation,&group.scope,group.due),|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let ceiling = || -> Result<Option<String>> {
        Ok(c.query_row("SELECT notice_id FROM master_notice_outbox WHERE conversation_id=?1 AND scope_id=?2 AND due_at=?3 AND flushed=0 ORDER BY notice_id DESC LIMIT 1",(&group.conversation,&group.scope,group.due),|r|r.get(0)).optional()?)
    };
    let page = |after: Option<&str>, cap: &str| -> Result<Vec<(String, String)>> {
        let mut q=c.prepare("SELECT notice_id,content FROM master_notice_outbox WHERE conversation_id=?1 AND scope_id=?2 AND due_at=?3 AND flushed=0 AND (?4 IS NULL OR notice_id>?4) AND notice_id<=?5 ORDER BY notice_id LIMIT 32")?;
        Ok(q.query_map(
            (&group.conversation, &group.scope, group.due, after, cap),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?)
    };
    let (mut members, mut cap) = match cursor.as_ref() {
        Some((after, cap)) => (page(Some(after), cap)?, Some(cap.as_str())),
        None => (Vec::new(), None),
    };
    let fresh_cap;
    if members.is_empty() {
        fresh_cap = ceiling()?;
        cap = fresh_cap.as_deref();
        if let Some(cap) = cap {
            members = page(None, cap)?;
        }
    }
    if let (Some((last, _)), Some(cap)) = (members.last(), cap) {
        c.execute("INSERT INTO master_notice_member_cursors VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(principal_id,conversation_id,scope_id,due_at) DO UPDATE SET notice_id=excluded.notice_id,round_notice_id=excluded.round_notice_id",(principal,&group.conversation,&group.scope,group.due,last,cap))?;
    }
    Ok(members)
}
pub(super) fn finish(c: &Connection, principal: &str, group: &Group) -> Result<()> {
    c.execute("DELETE FROM master_notice_member_cursors WHERE principal_id=?1 AND conversation_id=?2 AND scope_id=?3 AND due_at=?4 AND NOT EXISTS(SELECT 1 FROM master_notice_outbox WHERE conversation_id=?2 AND scope_id=?3 AND due_at=?4 AND flushed=0)",(principal,&group.conversation,&group.scope,group.due))?;
    Ok(())
}
