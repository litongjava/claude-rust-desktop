use super::*;
use crate::db::session_store::{Branch, SessionDb, Stores};

impl SessionStore<Stores> {
    pub fn create(&self, workspace: &Path, model: &str, provider: &str) -> Result<String> {
        self.create_with_id(
            workspace,
            model,
            provider,
            &uuid::Uuid::new_v4().to_string(),
        )
    }
    pub fn create_with_id(
        &self,
        workspace: &Path,
        model: &str,
        provider: &str,
        id: &str,
    ) -> Result<String> {
        Ok(self
            .db
            .create_code_session(workspace, model, provider, id)?
            .id()
            .to_owned())
    }
    pub fn resume(&self, id: &str) -> Result<conversation_repo::ConversationRow> {
        Ok(self.db.resume_code(id)?.1)
    }
    pub fn history(&self, id: &str) -> Result<Vec<Value>> {
        self.db.open_session(id)?.read(|r| {
            Ok(r.messages()?
                .into_iter()
                .map(|m| json!({"role":m.role,"content":m.content}))
                .collect())
        })
    }
    pub fn latest(&self, workspace: &Path) -> Result<Option<conversation_repo::ConversationRow>> {
        self.db.latest_code(workspace)
    }
    pub fn fork(&self, id: &str) -> Result<String> {
        self.branch(id, Branch::Full)
    }
    pub fn clear(&self, id: &str) -> Result<String> {
        self.branch(id, Branch::Clear)
    }
    fn branch(&self, id: &str, boundary: Branch<'_>) -> Result<String> {
        let (parent, _) = self.db.resume_code(id)?;
        let id = uuid::Uuid::new_v4().to_string();
        parent.branch(&id, boundary)?;
        Ok(id)
    }
    pub fn derive_owned_child(&self, id: &str, root: &Path, expected_cwd: &Path) -> Result<String> {
        let parent = self.session_in_root(id, root)?;
        let child = uuid::Uuid::new_v4().to_string();
        parent.derive_owned_child(&child, expected_cwd)?;
        Ok(child)
    }
    fn session_in_root(&self, id: &str, root: &Path) -> Result<SessionDb> {
        ensure!(
            std::path::absolute(root)? == self.db.data_dir().join("workspaces"),
            "Workspace root differs from the session data root"
        );
        self.db.open_session(id)
    }
    pub fn workspace(&self, id: &str, root: &Path) -> Result<PathBuf> {
        self.session_in_root(id, root)?.workspace()
    }
    pub fn model_history_with_boundary(
        &self,
        id: &str,
        format: &ApiFormat,
    ) -> Result<(Vec<Value>, Option<String>)> {
        self.db.open_session(id)?.read(|r| r.model_history(format))
    }
    pub fn model_history(&self, id: &str, format: &ApiFormat) -> Result<Vec<Value>> {
        Ok(self.model_history_with_boundary(id, format)?.0)
    }
    pub fn append(&self, id: &str, role: &str, text: &str) -> Result<String> {
        self.db.open_session(id)?.write(|w| w.append(role, text))
    }
    pub fn append_user_content(&self, id: &str, text: &str, content: &Value) -> Result<String> {
        self.db
            .open_session(id)?
            .write(|w| w.append_user_content(text, content))
    }
    pub fn append_hook_user(&self, id: &str, text: &str) -> Result<String> {
        self.db
            .open_session(id)?
            .write(|w| w.append_hook_user(text))
    }
}
