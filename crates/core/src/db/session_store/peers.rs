//! Peer discovery and inbox ownership are scoped to the visible incarnation.
use super::{directory, DirectoryFilter, Stores};
use crate::services::session_messages;
use anyhow::{Context, Result};
use serde_json::{json, Value};

impl Stores {
    pub(crate) fn peers(&self, id: &str, family: bool) -> Result<Vec<Value>> {
        let root = self.data_dir();
        let entries=self.global().read_directory_row(id,|c|{
            let tx=c.unchecked_transaction()?;
            let source=session_messages::project(&tx,root,id)?;
            let members=if family {
                let mut q=tx.prepare("WITH RECURSIVE edges(p,k) AS (
                        SELECT e.parent_id,e.child_id FROM session_execution_parents e JOIN conversations p ON (p.id,p.incarnation)=(e.parent_id,e.parent_incarnation)
                        UNION SELECT p.id,c.id FROM child_owners o JOIN conversations p ON p.incarnation=o.parent_incarnation JOIN conversations c ON c.incarnation=o.child_incarnation
                    ), family(id) AS (VALUES(?1) UNION SELECT k FROM edges JOIN family ON edges.p=family.id UNION SELECT p FROM edges JOIN family ON edges.k=family.id)
                    SELECT id FROM family")?;
                let rows=q.query_map([id],|r|r.get::<_,String>(0))?;
                Some(rows.collect::<rusqlite::Result<std::collections::HashSet<_>>>()?)
            }else{None};
            let rows=directory::list(&tx,DirectoryFilter{archived:Some(false),mode:Some("code"),..Default::default()})?;
            let mut found=Vec::new();
            for row in rows {
                let key=&row.conversation.id;
                if members.as_ref().is_some_and(|all|!all.contains(key)){continue;}
                if session_messages::project(&tx,root,key).is_ok_and(|other|other==source){found.push(row);}
            }
            Ok(Some(found))
        })?.context("Message session not found")?;
        let mut values = Vec::new();
        for entry in entries {
            let row = entry.conversation;
            values.push(json!({"id":row.id,"title":row.title,"current":row.id==id,"receiver_active":session_messages::receiver_active_by_key(root,&entry.incarnation)?,"saved":true}));
        }
        if family {
            values.sort_by_key(|row| {
                (
                    row["current"] != true,
                    row["id"].as_str().unwrap_or_default().to_owned(),
                )
            });
        }
        Ok(values)
    }
    pub(crate) fn message_receiver_active(&self, session: &str) -> Result<bool> {
        let row = self
            .global()
            .read_directory_row(session, |c| directory::one(c, session))?;
        row.map(|r| session_messages::receiver_active_by_key(self.data_dir(), &r.incarnation))
            .unwrap_or(Ok(false))
    }
}
