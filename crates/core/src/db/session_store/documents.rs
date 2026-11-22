//! Magic Docs keeps project registration and source digests in the directory.
use super::{shared::CatalogBodies, WRITE_DEADLINE};
use crate::services::magic_docs::{Document, ImportReport, Preview};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use std::path::{Path, PathBuf};

pub struct DocumentImport {
    pub root: PathBuf,
    pub path: PathBuf,
    pub title: String,
    pub updated: String,
    pub count: u64,
    pub source: String,
}
pub trait DocumentCatalog {
    fn track_document(&self, root: &Path, path: &Path, title: &str) -> Result<()>;
    fn documents(&self, root: &Path) -> Result<Vec<Document>>;
    fn forget_documents(&self, root: &Path) -> Result<usize>;
    fn document_auto(&self, root: &Path, id: &str, enabled: bool) -> Result<()>;
    fn document_previews(&self, root: &Path) -> Result<Vec<Preview>>;
    fn save_preview(&self, preview: &Preview) -> Result<()>;
    fn apply_preview(
        &self,
        preview: &Preview,
        path: &Path,
        staged: tempfile::NamedTempFile,
        rollback: tempfile::NamedTempFile,
    ) -> Result<()>;
    fn input_changed(&self, id: &str, input: &str) -> Result<bool>;
    fn record_input(&self, id: &str, input: &str) -> Result<()>;
    fn import_documents(
        &self,
        root: &Path,
        docs: &[DocumentImport],
        preview: bool,
    ) -> Result<ImportReport>;
}
impl<D: CatalogBodies> DocumentCatalog for D {
    fn track_document(&self, root: &Path, path: &Path, title: &str) -> Result<()> {
        self.directory_write("track document", title, WRITE_DEADLINE, |tx| {
            ensure!(
                tx.execute(
                    "INSERT INTO magic_documents(id,root,path,title) VALUES(?1,?2,?3,?4)
                ON CONFLICT(path) DO UPDATE SET title=excluded.title WHERE root=excluded.root",
                    params![
                        uuid::Uuid::new_v4().to_string(),
                        root.to_string_lossy(),
                        path.to_string_lossy(),
                        title
                    ]
                )? == 1,
                "Document is registered under a different project root"
            );
            Ok(())
        })
    }
    fn documents(&self, root: &Path) -> Result<Vec<Document>> {
        self.directory_read(|c|{
            let mut q=c.prepare("SELECT id,root,path,title,auto_update,updated_at,update_count FROM magic_documents WHERE root=?1 ORDER BY path")?;
            let rows=q.query_map([root.to_string_lossy().as_ref()],|r|Ok(Document{id:r.get(0)?,root:r.get(1)?,path:r.get(2)?,title:r.get(3)?,auto_update:r.get(4)?,updated_at:r.get(5)?,update_count:r.get(6)?}))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
    fn forget_documents(&self, root: &Path) -> Result<usize> {
        self.directory_write("clear documents", "", WRITE_DEADLINE, |tx| {
            Ok(tx.execute(
                "DELETE FROM magic_documents WHERE root=?1",
                [root.to_string_lossy().as_ref()],
            )?)
        })
    }
    fn document_auto(&self, root: &Path, id: &str, enabled: bool) -> Result<()> {
        self.directory_write("configure document updates", id, WRITE_DEADLINE, |tx| {
            ensure!(
                tx.execute(
                    "UPDATE magic_documents SET auto_update=?1 WHERE root=?2 AND id=?3",
                    params![enabled, root.to_string_lossy(), id]
                )? == 1,
                "Document is not in selected project"
            );
            Ok(())
        })
    }
    fn document_previews(&self, root: &Path) -> Result<Vec<Preview>> {
        let rows=self.directory_read(|c|{
            let mut q=c.prepare("SELECT p.id,p.document_id,p.original,p.proposed FROM magic_previews p JOIN magic_documents d ON d.id=p.document_id WHERE d.root=?1 ORDER BY p.created_at DESC")?;
            let rows=q.query_map([root.to_string_lossy().as_ref()], |r|Ok((r.get(0)?,r.get(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter()
            .map(|(id, document_id, old, new)| {
                let original = self.load_body(&old)?;
                let proposed = self.load_body(&new)?;
                let diff = diffy::create_patch(&original, &proposed).to_string();
                Ok(Preview {
                    id,
                    document_id,
                    original,
                    proposed,
                    diff,
                })
            })
            .collect()
    }
    fn save_preview(&self, p: &Preview) -> Result<()> {
        let old = self.store_body(&p.original)?;
        let new = self.store_body(&p.proposed)?;
        self.directory_write(
            "save document preview",
            &p.document_id,
            WRITE_DEADLINE,
            |tx| {
                tx.execute(
                    "DELETE FROM magic_previews WHERE document_id=?1",
                    [&p.document_id],
                )?;
                tx.execute(
                    "INSERT INTO magic_previews VALUES(?1,?2,?3,?4,?5)",
                    params![
                        p.id,
                        p.document_id,
                        old,
                        new,
                        chrono::Utc::now().to_rfc3339()
                    ],
                )?;
                Ok(())
            },
        )
    }
    fn apply_preview(
        &self,
        p: &Preview,
        path: &Path,
        staged: tempfile::NamedTempFile,
        rollback: tempfile::NamedTempFile,
    ) -> Result<()> {
        let mut staged = Some(staged);
        let mut published = false;
        let result=self.directory_write("apply document preview",&p.document_id,WRITE_DEADLINE,|tx|{
            ensure!(tx.execute("DELETE FROM magic_previews WHERE id=?1 AND document_id=?2",params![p.id,p.document_id])?==1,"Preview was removed");
            tx.execute("UPDATE magic_documents SET updated_at=?1,update_count=update_count+1 WHERE id=?2",params![chrono::Utc::now().to_rfc3339(),p.document_id])?;
            if let Some(file) = staged.take(){
                ensure!(path.canonicalize()?==path && std::fs::read_to_string(path)?==p.original,"Document changed before apply");
                file.persist(path)?;
                published=true;
            }else{
                ensure!(std::fs::read_to_string(path)?==p.proposed,"Document changed during apply");
            }
            Ok(())
        });
        if published && result.is_err() {
            ensure!(
                std::fs::read_to_string(path)? == p.proposed,
                "Document changed during rollback"
            );
            rollback
                .persist(path)
                .context("Database failed and file rollback failed")?;
        }
        result
    }
    fn input_changed(&self, id: &str, input: &str) -> Result<bool> {
        let key = self.body_reference(input)?;
        self.directory_read(|c| {
            Ok(!c
                .prepare_cached(
                    "SELECT 1 FROM magic_auto_inputs WHERE document_id=?1 AND input=?2",
                )?
                .exists([id, &key])?)
        })
    }
    fn record_input(&self, id: &str, input: &str) -> Result<()> {
        let key = self.store_body(input)?;
        self.directory_write("record document input",id,WRITE_DEADLINE,|c|{c.execute("INSERT INTO magic_auto_inputs VALUES(?1,?2) ON CONFLICT(document_id) DO UPDATE SET input=excluded.input",[id,&key])?;Ok(())})
    }
    fn import_documents(
        &self,
        root: &Path,
        docs: &[DocumentImport],
        preview: bool,
    ) -> Result<ImportReport> {
        let backup = if preview {
            None
        } else {
            Some(self.catalog_backup(root,"magic-docs",&["magic_documents","magic_previews","magic_auto_inputs","legacy_magic_imports"],
            "SELECT original FROM magic_previews UNION SELECT proposed FROM magic_previews UNION SELECT input FROM magic_auto_inputs UNION SELECT payload FROM legacy_magic_imports ORDER BY 1")?.to_string_lossy().into_owned())
        };
        let keys = docs
            .iter()
            .map(|d| {
                if preview {
                    self.body_reference(&d.source)
                } else {
                    self.store_body(&d.source)
                }
            })
            .collect::<Result<Vec<_>>>()?;
        self.directory_write("import tracked documents","",WRITE_DEADLINE,|tx|{
            let mut report=ImportReport{imported:0,skipped:0,preview,backup:backup.clone()};
            // A savepoint lets the dry run exercise uniqueness and foreign keys.
            tx.execute_batch("SAVEPOINT document_import")?;
            for (doc,key) in docs.iter().zip(&keys) {
                let path=doc.path.to_string_lossy();
                let old:Option<String>=tx.query_row("SELECT payload FROM legacy_magic_imports WHERE path=?1",[path.as_ref()],|r|r.get(0)).optional()?;
                if let Some(old)=old { ensure!(&old==key,"Legacy document changed since import");report.skipped+=1;continue; }
                let id=uuid::Uuid::new_v4().to_string();
                tx.execute("INSERT INTO magic_documents(id,root,path,title,auto_update,updated_at,update_count) VALUES(?1,?2,?3,?4,0,?5,?6)",params![id,doc.root.to_string_lossy(),path,doc.title,doc.updated,doc.count])?;
                tx.execute("INSERT INTO legacy_magic_imports VALUES(?1,?2,?3)",params![path,key,id])?;
                report.imported+=1;
            }
            if preview {tx.execute_batch("ROLLBACK TO document_import")?;}
            tx.execute_batch("RELEASE document_import")?;
            Ok(report)
        })
    }
}
