//! SDK control messages share the active engine; stdin remains live during turns.
use crate::{
    args::Cli,
    sdk_input::{Frame, UserMessage},
    session::Session,
};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::{oneshot, Mutex};

#[derive(Default)]
pub struct Permissions {
    pending: Mutex<HashMap<String, (Value, Option<String>, oneshot::Sender<Result<String>>)>>,
    closed: AtomicBool,
}
impl Permissions {
    pub async fn ask(
        &self,
        id: &str,
        tool_use_id: &str,
        tool: &str,
        input: &Value,
    ) -> Result<String> {
        self.request(id, tool_use_id, tool, input, None, None).await
    }

    pub async fn ask_question(
        &self,
        id: &str,
        tool_use_id: &str,
        question: &str,
        original: &Value,
    ) -> Result<String> {
        ensure!(
            original["question"] == question,
            "Active question does not match its tool input"
        );
        let options = original["options"]
            .as_array()
            .context("Question options must be an array")?;
        ensure!(
            original.get("multiSelect").is_none_or(Value::is_boolean),
            "Question multiSelect must be a boolean"
        );
        let options = options
            .iter()
            .map(|option| -> Result<Value> {
                ensure!(
                    option["label"]
                        .as_str()
                        .is_some_and(|label| !label.trim().is_empty()),
                    "Question option needs a nonempty label"
                );
                let mut option = option.clone();
                if option.get("description").is_none() {
                    option["description"] = json!("");
                }
                ensure!(
                    option["description"].is_string(),
                    "Question option description must be text"
                );
                Ok(option)
            })
            .collect::<Result<Vec<_>>>()?;
        let input = json!({"questions":[{"question":question,"header":"Question","options":options,"multiSelect":original.get("multiSelect").cloned().unwrap_or(json!(false))}]});
        self.request(
            id,
            tool_use_id,
            "AskUserQuestion",
            &input,
            Some(question.to_owned()),
            original["description"].as_str(),
        )
        .await
    }

    async fn request(
        &self,
        id: &str,
        tool_use_id: &str,
        tool: &str,
        input: &Value,
        question: Option<String>,
        description: Option<&str>,
    ) -> Result<String> {
        ensure!(
            !self.closed.load(Ordering::SeqCst),
            "SDK input closed before tool approval"
        );
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(id.into(), (input.clone(), question, tx));
        let mut frame = json!({"type":"control_request","request_id":id,"request":{"subtype":"can_use_tool","tool_name":tool,"input":input,"tool_use_id":tool_use_id}});
        if let Some(description) = description {
            frame["request"]["description"] = json!(description);
        }
        crate::session::emit_json(&frame)?;
        rx.await.context("SDK permission channel closed")?
    }
    pub async fn cancel(&self) {
        for (id, (_, _, tx)) in self.pending.lock().await.drain() {
            let _ = tx.send(Err(anyhow::anyhow!("SDK turn interrupted")));
            let _ = crate::session::emit_json(
                &json!({"type":"control_cancel_request","request_id":id}),
            );
        }
    }
    async fn respond(&self, value: &Value) -> Result<()> {
        let response = &value["response"];
        let id = response["request_id"]
            .as_str()
            .context("Missing response request_id")?;
        let Some((input, question, tx)) = self.pending.lock().await.remove(id) else {
            bail!("Unknown or completed SDK permission request: {id}")
        };
        let answer = (|| -> Result<String> {
            ensure!(
                response["subtype"] == "success",
                "SDK host rejected permission request: {}",
                response["error"]
            );
            let result = &response["response"];
            ensure!(
                result
                    .get("updatedPermissions")
                    .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
                "SDK updatedPermissions is not supported; no permission was granted"
            );
            match result["behavior"].as_str() {
                Some("allow") => {
                    if let Some(question) = question.as_deref() {
                        return question_answer(&input, question, &result["updatedInput"]);
                    }
                    ensure!(result.get("updatedInput").is_none_or(|v|v==&input),"SDK updatedInput differs from reviewed tool input; no permission was granted");
                    Ok("yes".into())
                }
                Some("deny") if question.is_some() => bail!("SDK host declined the user question"),
                Some("deny") => Ok("no".into()),
                _ => bail!("SDK permission response needs behavior allow or deny"),
            }
        })();
        let _ = tx.send(answer);
        Ok(())
    }
}

fn question_answer(input: &Value, question: &str, updated: &Value) -> Result<String> {
    let mut fields = updated
        .as_object()
        .context("AskUserQuestion approval needs updatedInput with answers")?
        .clone();
    let answers = fields
        .remove("answers")
        .context("AskUserQuestion approval needs answers")?;
    ensure!(
        Value::Object(fields) == *input,
        "AskUserQuestion updatedInput may add answers but cannot change questions or options"
    );
    let answers = answers
        .as_object()
        .context("AskUserQuestion answers must map question text to answer strings")?;
    ensure!(
        answers.len() == 1,
        "SDK must answer exactly the requested question"
    );
    let answer = answers
        .get(question)
        .and_then(Value::as_str)
        .context("SDK answer does not match the requested question")?;
    ensure!(
        !answer.trim().is_empty() && answer.len() <= 64 * 1024,
        "SDK answer must be nonempty and at most 64 KiB"
    );
    Ok(answer.to_owned())
}

#[cfg(test)]
mod question_tests {
    use super::*;
    #[test]
    fn answer_requires_exact_question_and_unchanged_input() {
        let input =
            json!({"questions":[{"question":"Language?","multiSelect":false,"options":[]}]});
        let mut updated = input.clone();
        updated["answers"] = json!({"Language?":"自定义 Rust"});
        assert_eq!(
            question_answer(&input, "Language?", &updated).unwrap(),
            "自定义 Rust"
        );
        for answers in [
            json!({}),
            json!({"Other":"Rust"}),
            json!({"Language?":3}),
            json!({"Language?":""}),
            json!({"Language?":"Rust","Other":"Go"}),
        ] {
            updated["answers"] = answers;
            assert!(question_answer(&input, "Language?", &updated).is_err());
        }
        updated["answers"] = json!({"Language?":"Rust"});
        updated["questions"][0]["multiSelect"] = json!(true);
        assert!(question_answer(&input, "Language?", &updated).is_err());
    }
}
fn response(id: &Value, result: Result<Value>) -> Result<()> {
    crate::session::emit_json(&match result {
        Ok(value) => {
            json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":value}})
        }
        Err(error) => {
            json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":format!("{error:#}")}})
        }
    })
}
fn initialize(session: &Session, request: &Value) -> Result<Value> {
    for (key, value) in request.as_object().context("request must be an object")? {
        if key == "subtype" {
            continue;
        }
        ensure!(
            value.is_null() || value == &json!({}) || value == &json!([]) || value == &json!(false),
            "initialize.{key} is not supported; use CLI configuration where available"
        );
    }
    Ok(
        json!({"commands":[],"agents":[],"output_style":"default","available_output_styles":["default"],"models":[{"value":session.model,"displayName":session.model,"description":"Current configured model"}],"account":{},"pid":std::process::id()}),
    )
}
async fn idle_control(session: &mut Session, frame: &Value) -> Result<()> {
    let request = &frame["request"];
    let result = async {
        match request["subtype"].as_str().unwrap_or("") {
            "initialize" => initialize(session, request),
            "interrupt" => Ok(json!({})),
            "set_permission_mode" => {
                let mode = match request["mode"].as_str().context("mode is required")? {
                    "default" => "ask",
                    "acceptEdits" => "accept-edits",
                    "bypassPermissions" => "bypass",
                    other => other,
                };
                ensure!(
                    request.get("ultraplan").is_none_or(|v| v == false),
                    "ultraplan is not supported"
                );
                session.set_permission_mode(mode).await?;
                Ok(json!({}))
            }
            "set_model" => {
                let model = request["model"]
                    .as_str()
                    .context("An explicit configured model is required")?;
                session.sdk_set_model(model).await?;
                Ok(json!({}))
            }
            other => bail!("Unsupported SDK control request: {other}"),
        }
    }
    .await;
    response(&frame["request_id"], result)
}
pub async fn run(cli: &Cli, session: &mut Session, initial: Option<&str>) -> Result<()> {
    let permissions = Arc::new(Permissions::default());
    session.sdk_permissions = Some(permissions.clone());
    let mut frames = crate::sdk_input::stdin();
    let mut queue = VecDeque::<UserMessage>::new();
    if let Some(text) = initial {
        queue.push_back(crate::sdk_input::parse_line(
            &json!({"type":"user","message":{"role":"user","content":text}}).to_string(),
        )?);
    }
    let mut eof = false;
    let mut failed = false;
    loop {
        if let Some(message) = queue.pop_front() {
            ensure!(
                message
                    .session_id
                    .as_deref()
                    .is_none_or(|id| id.is_empty() || id == session.id),
                "Input session_id differs from active session"
            );
            if cli.replay_user_messages {
                let mut envelope = message.envelope;
                envelope["session_id"] = json!(session.id);
                crate::session::emit_json(&envelope)?;
            }
            let mut terminal = None;
            session.sdk_content = message
                .content
                .as_array()
                .filter(|blocks| blocks.iter().any(|b| b["type"] == "image_url"))
                .map(|_| message.content.clone());
            let turn = Box::pin(session.turn(cli, &message.text, &mut terminal));
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    result=&mut turn=>{if let Err(error)=result {failed=true;if !error.is::<crate::output::ReportedError>() {crate::session::emit_json(&json!({"type":"result","is_error":true,"session_id":session.id,"error":format!("{error:#}")}))?;}}break;},
                    frame=frames.recv(),if !eof=>match frame {
                        None=>{eof=true;permissions.closed.store(true,Ordering::SeqCst);permissions.cancel().await;},
                        Some(Err(error))=>{permissions.cancel().await;session.engine.cancel_turn(&session.id).await;return Err(error)},
                        Some(Ok(Frame::User(user)))=>{ensure!(queue.len()<32,"SDK pending user queue exceeds 32 messages");queue.push_back(user);},
                        Some(Ok(Frame::Response(value)))=>{permissions.respond(&value).await?;},
                        Some(Ok(Frame::Control(value)))=>{
                            if value["request"]["subtype"]=="interrupt" {
                                failed=true;
                                permissions.cancel().await;session.engine.cancel_turn(&session.id).await;
                                response(&value["request_id"],Ok(json!({})))?;
                                crate::session::emit_json(&json!({"type":"result","is_error":true,"session_id":session.id,"error":"SDK turn interrupted"}))?;
                                break;
                            }
                            response(&value["request_id"],Err(anyhow::anyhow!("Control changes require an idle session; interrupt the active turn first")))?;
                        },
                    },
                }
            }
            permissions.cancel().await;
            continue;
        }
        if eof {
            break;
        }
        match tokio::select! {frame=frames.recv()=>frame,_=tokio::signal::ctrl_c()=>bail!("SDK input interrupted")}
        {
            None => eof = true,
            Some(Err(error)) => return Err(error),
            Some(Ok(Frame::User(user))) => queue.push_back(user),
            Some(Ok(Frame::Control(value))) => idle_control(session, &value).await?,
            Some(Ok(Frame::Response(value))) => permissions.respond(&value).await?,
        }
    }
    session.sdk_permissions = None;
    session.sdk_content = None;
    if failed {
        return Err(crate::output::ReportedError(
            "One or more SDK turns failed or were interrupted".into(),
        )
        .into());
    }
    Ok(())
}
