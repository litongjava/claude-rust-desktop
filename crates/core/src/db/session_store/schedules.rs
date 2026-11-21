use super::shared::DirectoryRepository;
use crate::db::task_repo::{self as repo, ScheduledTaskRow, TaskRunRow};
use anyhow::Result;

pub trait Schedules: Send + Sync {
    fn insert_scheduled_task(
        &self,
        id: &str,
        name: &str,
        description: Option<&str>,
        cron_expression: &str,
        task_type: &str,
        task_config: &str,
        conversation_id: Option<&str>,
        is_enabled: bool,
        next_run_at: Option<&str>,
        created_at: &str,
        updated_at: &str,
    ) -> Result<()>;
    fn list_scheduled_tasks(&self) -> Result<Vec<ScheduledTaskRow>>;
    fn get_scheduled_task(&self, id: &str) -> Result<Option<ScheduledTaskRow>>;
    fn update_scheduled_task(
        &self,
        id: &str,
        name: Option<&str>,
        description: Option<&str>,
        cron_expression: Option<&str>,
        task_type: Option<&str>,
        task_config: Option<&str>,
        conversation_id: Option<&str>,
        is_enabled: Option<bool>,
        next_run_at: Option<&str>,
        updated_at: &str,
    ) -> Result<()>;
    fn delete_scheduled_task(&self, id: &str) -> Result<()>;
    fn get_due_tasks(&self, now: &str) -> Result<Vec<ScheduledTaskRow>>;
    fn update_task_run_result(
        &self,
        id: &str,
        last_run_at: &str,
        last_run_status: &str,
        last_run_output: Option<&str>,
        next_run_at: Option<&str>,
    ) -> Result<()>;
    fn get_enabled_tasks(&self) -> Result<Vec<ScheduledTaskRow>>;
    fn insert_task_run(&self, id: &str, task_id: &str, started_at: &str) -> Result<()>;
    fn update_task_run_status(
        &self,
        id: &str,
        status: &str,
        output: Option<&str>,
        error_message: Option<&str>,
        finished_at: &str,
        duration_ms: i64,
    ) -> Result<()>;
    fn get_task_runs(&self, task_id: &str, limit: usize) -> Result<Vec<TaskRunRow>>;
}
impl<D: DirectoryRepository + Send + Sync> Schedules for D {
    fn insert_scheduled_task(
        &self,
        id: &str,
        name: &str,
        description: Option<&str>,
        cron_expression: &str,
        task_type: &str,
        task_config: &str,
        conversation_id: Option<&str>,
        is_enabled: bool,
        next_run_at: Option<&str>,
        created_at: &str,
        updated_at: &str,
    ) -> Result<()> {
        self.directory_write("insert scheduled task", id, super::WRITE_DEADLINE, |c| {
            repo::insert_scheduled_task(
                c,
                id,
                name,
                description,
                cron_expression,
                task_type,
                task_config,
                conversation_id,
                is_enabled,
                next_run_at,
                created_at,
                updated_at,
            )
        })
    }
    fn list_scheduled_tasks(&self) -> Result<Vec<ScheduledTaskRow>> {
        self.directory_read(|c| repo::list_scheduled_tasks(c))
    }
    fn get_scheduled_task(&self, id: &str) -> Result<Option<ScheduledTaskRow>> {
        self.directory_read(|c| repo::get_scheduled_task(c, id))
    }
    fn update_scheduled_task(
        &self,
        id: &str,
        name: Option<&str>,
        description: Option<&str>,
        cron_expression: Option<&str>,
        task_type: Option<&str>,
        task_config: Option<&str>,
        conversation_id: Option<&str>,
        is_enabled: Option<bool>,
        next_run_at: Option<&str>,
        updated_at: &str,
    ) -> Result<()> {
        self.directory_write("update scheduled task", id, super::WRITE_DEADLINE, |c| {
            repo::update_scheduled_task(
                c,
                id,
                name,
                description,
                cron_expression,
                task_type,
                task_config,
                conversation_id,
                is_enabled,
                next_run_at,
                updated_at,
            )
        })
    }
    fn delete_scheduled_task(&self, id: &str) -> Result<()> {
        self.directory_write("delete scheduled task", id, super::WRITE_DEADLINE, |c| {
            repo::delete_scheduled_task(c, id)
        })
    }
    fn get_due_tasks(&self, now: &str) -> Result<Vec<ScheduledTaskRow>> {
        self.directory_read(|c| repo::get_due_tasks(c, now))
    }
    fn update_task_run_result(
        &self,
        id: &str,
        last_run_at: &str,
        last_run_status: &str,
        last_run_output: Option<&str>,
        next_run_at: Option<&str>,
    ) -> Result<()> {
        self.directory_write("update task run result", id, super::WRITE_DEADLINE, |c| {
            repo::update_task_run_result(
                c,
                id,
                last_run_at,
                last_run_status,
                last_run_output,
                next_run_at,
            )
        })
    }
    fn get_enabled_tasks(&self) -> Result<Vec<ScheduledTaskRow>> {
        self.directory_read(|c| repo::get_enabled_tasks(c))
    }
    fn insert_task_run(&self, id: &str, task_id: &str, started_at: &str) -> Result<()> {
        self.directory_write("insert task run", id, super::WRITE_DEADLINE, |c| {
            repo::insert_task_run(c, id, task_id, started_at)
        })
    }
    fn update_task_run_status(
        &self,
        id: &str,
        status: &str,
        output: Option<&str>,
        error_message: Option<&str>,
        finished_at: &str,
        duration_ms: i64,
    ) -> Result<()> {
        self.directory_write("update task run status", id, super::WRITE_DEADLINE, |c| {
            repo::update_task_run_status(
                c,
                id,
                status,
                output,
                error_message,
                finished_at,
                duration_ms,
            )
        })
    }
    fn get_task_runs(&self, task_id: &str, limit: usize) -> Result<Vec<TaskRunRow>> {
        self.directory_read(|c| repo::get_task_runs(c, task_id, limit))
    }
}
