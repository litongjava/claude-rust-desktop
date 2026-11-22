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
        self.reload_with_cut(shard, false).map(drop)
    }

    fn reload_with_cut(
        &mut self,
        shard: &crate::db::session_store::SessionDb,
        capture_cut: bool,
    ) -> Result<Option<crate::db::session_store::CompactionWindow>> {
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
        let (mut messages, boundary, (index, entries), (protected, pending), cut) =
            shard.read(|r| {
                let (messages, boundary) = r.model_history(&self.provider.provider.api_format)?;
                let context = crate::services::turn_context::replay_at(
                    shard,
                    r,
                    boundary.as_deref(),
                    &messages,
                )?;
                let protection = r.protected_inputs(&messages)?;
                let cut = capture_cut
                    .then(|| {
                        r.compaction_window(
                            &messages,
                            protection.0,
                            self.parent_message_id.as_deref(),
                            &self.provider.provider.api_format,
                        )
                    })
                    .transpose()?;
                Ok((messages, boundary, context, protection, cut))
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
        self.session_reminders.resident_indexes = index;
        self.session_reminders.skills =
            crate::services::turn_context::invoked_skills_reminder(&entries, &messages);
        self.context_reminders =
            crate::prompt::first_message_reminders(self.session_reminders.clone());
        self.turn_context = entries;
        self.messages = messages;
        self.applied_compaction = boundary.is_some();
        self.context_boundary_id = boundary;
        Ok(cut)
    }

    pub(super) fn reload_session_history(&mut self) -> Result<()> {
        let Some(ExecutorHistory::Session(shard)) = self.executor_history.clone() else {
            anyhow::bail!("Admitted input requires durable session history");
        };
        self.reload_history(&shard)
    }

    async fn compact_native_window(
        &self,
        shard: &crate::db::session_store::SessionDb,
        cut: &crate::db::session_store::CompactionWindow,
        mut input: Vec<Value>,
    ) -> Result<()> {
        use crate::native_engine::request_recording::CompactionRecord;
        let mut options = self
            .openai_client
            .request_options
            .clone()
            .compaction("native");
        options.compaction_metrics = Some(super::super::compaction_metrics::MetricsStore(
            shard.clone(),
        ));
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
        let boundary = shard.commit_compaction_window(cut, &compacted)?;
        if let Some(record) = &mut record {
            record.finish(&compacted, &boundary);
        }
        Ok(())
    }

    // A live window can refresh its durable prefix. A summary already owns
    // a snapshot, possibly cut before a queued input or at a partial boundary.
    pub(super) async fn replay_window<M: serde::Serialize + serde::de::DeserializeOwned>(
        &mut self,
        window: &mut Vec<M>,
        durable_len: Option<usize>,
        render: fn(&Self, &[(Vec<String>, Vec<String>)]) -> Result<Vec<M>>,
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
        let mut physical_cut = None;
        let mut persisted_len = window.len();
        if let (Some(durable_len), Some(ExecutorHistory::Session(shard))) =
            (durable_len, self.executor_history.clone())
        {
            let additions = window.split_off(durable_len);
            physical_cut = self.reload_with_cut(&shard, true)?;
            *window = render(self, &self.message_additions())?;
            persisted_len = window.len();
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

        if native {
            if durable_len.is_none() {
                return Ok(());
            }
            let Some(ExecutorHistory::Session(shard)) = self.executor_history.clone() else {
                anyhow::bail!("Native Responses compression requires durable session history");
            };
            let cut = physical_cut.context("Missing native compaction snapshot")?;
            if cut.prefix_len > 0 {
                // The Responses renderer can insert one standalone index after
                // an opaque boundary when no later receipt supersedes it.
                let extra_index = persisted_len.saturating_sub(self.messages.len());
                let end = cut.prefix_len + extra_index;
                anyhow::ensure!(end <= persisted_len, "Invalid native compaction cut");
                let mut suffix = rows.split_off(end);
                if TokenCounter::estimate(&suffix) > capacity {
                    crate::services::compaction::clear_tool_results(&mut suffix);
                    anyhow::ensure!(TokenCounter::estimate(&suffix) <= capacity,
                        "Context overflow: the current turn's admitted inputs exceed the model window");
                }
                self.compact_native_window(&shard, &cut, rows).await?;
                let additions = window.split_off(persisted_len);
                self.reload_history(&shard)?;
                *window = render(self, &self.message_additions())?;
                window.extend(additions);
            }
            let mut result = values(window)?;
            if TokenCounter::estimate(&result) > capacity {
                crate::services::compaction::clear_tool_results(&mut result);
                anyhow::ensure!(TokenCounter::estimate(&result) <= capacity,
                    "Context overflow: the admitted inputs and required context exceed the model window");
                *window = result
                    .into_iter()
                    .map(serde_json::from_value)
                    .collect::<serde_json::Result<_>>()?;
            }
            return Ok(());
        }

        // A generated text summary is a durable boundary, just like a native
        // Responses window. Assign the cut to an actual transcript row before
        // committing so a retained tool call and its results remain together.
        if let (Some(original), Some(ExecutorHistory::Session(shard))) =
            (physical_cut, self.executor_history.clone())
        {
            let end = self.protected_input.unwrap_or_else(|| {
                self.messages
                    .iter()
                    .rposition(|m| m["role"] == "user")
                    .unwrap_or(self.messages.len())
            });
            if let Some(mut at) = compressor.retained_input(&self.messages, end, &level) {
                if TokenCounter::estimate(&rows[at..]) > capacity {
                    at = end;
                }
                let cut = shard.read(|r| {
                    r.compaction_window(
                        &self.messages,
                        (at < self.messages.len()).then_some(at),
                        self.parent_message_id.as_deref(),
                        &self.provider.provider.api_format,
                    )
                })?;
                anyhow::ensure!(
                    cut.fingerprint == original.fingerprint,
                    "Session changed during compaction"
                );
                if cut.prefix_len > 0 {
                    let mut prefix = self.messages[..cut.prefix_len].to_vec();
                    prefix.retain(|m| m["origin"] != "memory_change");
                    let text = compressor.window_summary(&prefix, &level).await;
                    shard.commit_compaction_window(&cut, &text.as_str().into())?;
                    let tail = window.split_off(persisted_len);
                    self.reload_history(&shard)?;
                    *window = render(self, &self.message_additions())?;
                    window.extend(tail);
                    let mut rebuilt = values(window)?;
                    if TokenCounter::estimate(&rebuilt) > capacity {
                        crate::services::compaction::clear_tool_results(&mut rebuilt);
                        anyhow::ensure!(TokenCounter::estimate(&rebuilt) <= capacity,"Context overflow: the admitted inputs and required context exceed the model window");
                        *window = rebuilt
                            .into_iter()
                            .map(serde_json::from_value)
                            .collect::<serde_json::Result<_>>()?;
                    }
                    return Ok(());
                }
            }
        }

        // Compress only the history preceding the current typed exchange. Its
        // multimodal blocks, receipts and any following steers travel together.
        let protected = self.protected_input;
        let cut = protected.unwrap_or(rows.len()).min(rows.len());
        let prefix_index = match &self.executor_history {
            Some(ExecutorHistory::Session(shard)) => shard.read(|r| {
                crate::services::memory_files::prompt::visible_indexes(
                    r,
                    self.context_boundary_id.as_deref(),
                    &self.messages,
                )
            })?,
            _ => self.session_reminders.resident_indexes.clone(),
        };
        let prefix_index = (!prefix_index.is_empty())
            .then(|| json!({"role":"user","content":prefix_index.join("\n\n")}));
        let mut suffix = rows.split_off(cut);
        self.remove_compaction_indexes(&mut rows);
        self.remove_compaction_indexes(&mut suffix);
        if protected.is_some() && TokenCounter::estimate(&suffix) > capacity {
            crate::services::compaction::clear_tool_results(&mut suffix);
            if durable_len.is_none() && TokenCounter::estimate(&suffix) > capacity {
                // The summary planner can split an input too large for a normal turn.
                return Ok(());
            }
            anyhow::ensure!(
                TokenCounter::estimate(&suffix) <= capacity,
                "Context overflow: the current turn's admitted inputs exceed the model window"
            );
        }
        let mut compressed = if rows.is_empty() {
            rows
        } else if tiered && level == CompressionLevel::None {
            let result = crate::memory::TieredCompressor::new().compress(&rows);
            if rows == result && estimated <= capacity {
                return Ok(());
            }
            result
        } else {
            let (result, metadata) = compressor
                .compress(rows.clone(), None, level, Some(&self.provider))
                .await;
            if metadata.was_compressed {
                result
            } else if estimated <= capacity {
                // An unchanged window keeps the normal request's cached prefix.
                return Ok(());
            } else {
                rows
            }
        };
        let index_at = prefix_index.map(|index| {
            let at = compressed
                .iter()
                .take_while(|row| row["role"] == "system")
                .count();
            compressed.insert(at, index);
            at
        });
        let suffix_start = compressed.len();
        compressed.extend(suffix);
        if protected.is_some() && TokenCounter::estimate(&compressed) > capacity {
            // Discard older exchanges before sacrificing current-turn tool results.
            compressed = compressed
                .into_iter()
                .enumerate()
                .filter_map(|(i, row)| {
                    (i >= suffix_start
                        || Some(i) == index_at
                        || row["role"] == "system"
                        || row["_is_summary"] == true)
                        .then_some(row)
                })
                .collect();
            if TokenCounter::estimate(&compressed) > capacity {
                crate::services::compaction::clear_tool_results(&mut compressed);
                if durable_len.is_none() && TokenCounter::estimate(&compressed) > capacity {
                    return Ok(());
                }
                anyhow::ensure!(TokenCounter::estimate(&compressed) <= capacity,
                    "Context overflow: the admitted inputs and required context exceed the model window");
            }
        }
        *window = compressed
            .into_iter()
            .map(serde_json::from_value)
            .collect::<serde_json::Result<_>>()?;
        Ok(())
    }
}
