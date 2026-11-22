//! Rebuild provider input from the same session view used on resume.
use super::*;

impl ToolLoopExecutor {
    pub(super) fn acknowledge_model_inputs(&mut self) -> Result<()> {
        if let Some(ExecutorHistory::Session(shard)) = &self.executor_history {
            shard.acknowledge_model_inputs(&self.pending_inputs)?;
        }
        self.pending_inputs.clear();
        Ok(())
    }

    pub(super) fn boundary_changed(&self) -> Result<bool> {
        match &self.executor_history {
            Some(ExecutorHistory::Session(shard)) => {
                Ok(shard.read(|r| r.boundary_id())? != self.context_boundary_id)
            }
            _ => Ok(false),
        }
    }

    pub(super) fn retain_replay_tool(
        &mut self,
        call: &PendingToolCall,
        input: &Value,
        output: &str,
        is_error: bool,
    ) {
        let (Some(ExecutorHistory::Session(_)), Some(segment)) =
            (&self.executor_history, &self.parent_message_id)
        else {
            return;
        };
        let row = crate::db::message_repo::ToolCallRow {
            id: call.id.clone(),
            message_id: segment.clone(),
            name: call.name.clone(),
            input: Some(input.to_string()),
            output: Some(output.into()),
            is_error,
            sort_order: self.tool_order,
        };
        self.tool_order += 1;
        self.replay_tools.push(row);
    }

    pub(super) fn reload_history(
        &mut self,
        shard: &crate::db::session_store::SessionDb,
    ) -> Result<()> {
        if let Some(segment) = &self.parent_message_id {
            if !self.replay_tools.is_empty() {
                ExecutorHistory::Session(shard.clone()).save_segment(
                    segment,
                    crate::db::session_store::SegmentUpdate {
                        tools: &self.replay_tools,
                        ..Default::default()
                    },
                )?;
                self.replay_tools.clear();
            }
        }
        let (mut messages, boundary, (index, entries), (protected, pending)) = shard.read(|r| {
            let (messages, boundary) = r.model_history(&self.provider.provider.api_format)?;
            let context =
                crate::services::turn_context::replay_at(shard, r, boundary.as_deref(), &messages)?;
            let protection = r.protected_inputs(&messages)?;
            Ok((messages, boundary, context, protection))
        })?;
        if self.standalone_frontend {
            crate::services::compaction::microcompact(
                shard,
                boundary.as_deref(),
                &mut messages,
                self.provider.model.context_window,
            )?;
        }
        self.protected_input = protected;
        self.pending_inputs = pending;
        self.session_reminders.project_memory = index;
        self.session_reminders.skills =
            crate::services::turn_context::invoked_skills_reminder(&entries, &messages);
        self.context_reminders =
            crate::prompt::first_message_reminders(self.session_reminders.clone());
        self.turn_context = entries;
        self.messages = messages;
        self.applied_compaction = boundary.is_some();
        self.context_boundary_id = boundary;
        Ok(())
    }

    pub(super) fn reload_session_history(&mut self) -> Result<()> {
        let Some(ExecutorHistory::Session(shard)) = self.executor_history.clone() else {
            anyhow::bail!("Admitted input requires durable session history");
        };
        self.reload_history(&shard)
    }

    async fn compact_native_window(&self, mut input: Vec<Value>) -> Result<Vec<Value>> {
        use crate::native_engine::request_recording::CompactionRecord;
        let mut options = self
            .openai_client
            .request_options
            .clone()
            .compaction("native");
        let mut record =
            CompactionRecord::start(&mut options, "auto", &serde_json::to_string(&input)?);
        self.remove_compaction_indexes(&mut input);
        let compacted = crate::services::compaction::native::generate_messages_with_options(
            &self.provider,
            options,
            input
                .into_iter()
                .map(serde_json::from_value)
                .collect::<serde_json::Result<_>>()?,
            self.system_prompt.as_deref().unwrap_or(""),
            self.conv_id.as_deref(),
        )
        .await?;
        if let Some(record) = &mut record {
            record.finish(&compacted, "in-flight-native-window");
        }
        let mut result = vec![
            json!({"role":"user", "content":compacted.text, "provider_content":compacted.native}),
        ];
        if let Some(index) = self.compacted_index() {
            result.push(json!({"role":"user","content":index}));
        }
        Ok(result)
    }

    pub(super) async fn replay_window<M: serde::Serialize + serde::de::DeserializeOwned>(
        &mut self,
        window: &mut Vec<M>,
        durable_len: usize,
        render: fn(&Self) -> Result<Vec<M>>,
    ) -> Result<()> {
        let model = &self.provider.model;
        let limit = model
            .context_window
            .map(|n| (n as f64 * 0.90) as usize)
            .unwrap_or_else(|| TokenCounter::get_safe_limit(&model.id, 0.90));
        let values = |messages: &[M]| {
            messages
                .iter()
                .map(serde_json::to_value)
                .collect::<serde_json::Result<Vec<_>>>()
        };
        let mut rows = values(window)?;
        let native = crate::services::compaction::native::supports(&self.provider);
        let threshold = crate::config::OrchestrationConfig::token_threshold();
        let capacity = model
            .context_window
            .map(|n| n as usize)
            .unwrap_or_else(|| TokenCounter::get_model_context_limit(&model.id))
            .saturating_sub(self.max_tokens as usize);
        let estimated = TokenCounter::estimate(&rows);
        if estimated <= (limit * 70 / 100).min(capacity) && (native || estimated <= threshold) {
            return Ok(());
        }
        if let Some(ExecutorHistory::Session(shard)) = self.executor_history.clone() {
            let additions = window.split_off(durable_len);
            self.reload_history(&shard)?;
            *window = render(self)?;
            window.extend(additions);
            rows = values(window)?;
        }
        let compressor = ContextCompressor::new(None);
        let estimated = TokenCounter::estimate(&rows);
        let level = compressor.determine_compression_level(&ContextWindow {
            model_id: self.provider.model.id.clone(),
            total_tokens: estimated as u64,
            reserved_for_completion: (self.max_tokens as u64) * 2,
            context_window: limit as u64,
        });
        let tiered = !native && estimated > threshold;
        if level == CompressionLevel::None && !tiered && estimated <= capacity {
            return Ok(());
        }

        // Compress only the history preceding the current typed exchange. Its
        // multimodal blocks, receipts and any following steers travel together.
        // Responses renders its opaque boundary and frozen memory as separate rows.
        let protected = self.protected_input.map(|at| {
            at + usize::from(
                native
                    && at > 0
                    && self
                        .messages
                        .first()
                        .is_some_and(|m| m["provider_content"]["compaction"] == true)
                    && self.session_reminders.project_memory.is_some(),
            )
        });
        let mut suffix = rows.split_off(protected.unwrap_or(rows.len()).min(rows.len()));
        if protected.is_some() && TokenCounter::estimate(&suffix) > capacity {
            crate::services::compaction::clear_tool_results(&mut suffix);
            anyhow::ensure!(
                TokenCounter::estimate(&suffix) <= capacity,
                "Context overflow: the current turn's admitted inputs exceed the model window"
            );
        }
        let mut compressed = if rows.is_empty() {
            Vec::new()
        } else if native {
            self.compact_native_window(rows).await?
        } else {
            let original = rows.clone();
            self.remove_compaction_indexes(&mut rows);
            let (mut result, changed) = if tiered && level == CompressionLevel::None {
                let result = crate::memory::TieredCompressor::new().compress(&rows);
                let changed = rows != result;
                (result, changed)
            } else {
                let (result, metadata) = compressor
                    .compress(rows, None, level, Some(&self.provider))
                    .await;
                (result, metadata.was_compressed)
            };
            if changed {
                self.add_compacted_memory(&mut result);
                result
            } else {
                original
            }
        };
        if !native
            && protected.is_some()
            && TokenCounter::estimate(&compressed) + TokenCounter::estimate(&suffix) > capacity
        {
            // Keep the summary and system context when the compressor's last
            // historical exchange would crowd out current-turn input.
            compressed.retain(|row| row["role"] == "system" || row["_is_summary"] == true);
            self.add_compacted_memory(&mut compressed);
        }
        compressed.extend(suffix);
        anyhow::ensure!(
            protected.is_none() || TokenCounter::estimate(&compressed) <= capacity,
            "Context overflow: the admitted inputs and required context exceed the model window"
        );
        *window = compressed
            .into_iter()
            .map(serde_json::from_value)
            .collect::<serde_json::Result<_>>()?;
        Ok(())
    }
}
