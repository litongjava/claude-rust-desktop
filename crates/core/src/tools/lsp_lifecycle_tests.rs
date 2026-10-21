use super::*;

const FIXTURE: &str = r#"
const fs=require('fs'), cp=require('child_process');
const root=process.env.LSP_FIXTURE_ROOT;
const log=value=>fs.appendFileSync(root+'/events.jsonl',JSON.stringify({...value,pid:process.pid})+'\n');
const control=()=>JSON.parse(fs.readFileSync(root+'/control.json','utf8'));
log({event:'spawn'});
if(control().spawnChild){const child=cp.spawn(process.execPath,['-e','setInterval(()=>{},10000)'],{stdio:'ignore',windowsHide:true});log({event:'child',child:child.pid});}
let buffer=Buffer.alloc(0),rootUri;
const reply=(id,result)=>{const body=JSON.stringify({jsonrpc:'2.0',id,result});process.stdout.write('Content-Length: '+Buffer.byteLength(body)+'\r\n\r\n'+body);};
process.stdin.on('data',chunk=>{buffer=Buffer.concat([buffer,chunk]);while(true){
 const end=buffer.indexOf('\r\n\r\n');if(end<0)return;
 const length=Number(buffer.subarray(0,end).toString().match(/Content-Length: (\d+)/i)[1]);
 if(buffer.length<end+4+length)return;
 const message=JSON.parse(buffer.subarray(end+4,end+4+length));buffer=buffer.subarray(end+4+length);
 log({event:message.method});const options=control();
 if(message.method==='initialize'){
  rootUri=message.params.rootUri;
  if(options.mode==='crashInit')process.exit(17);
  setTimeout(()=>reply(message.id,{capabilities:{}}),options.mode==='slowInit'?options.delay:0);
 }else if(message.method==='textDocument/hover'){
  if(options.mode==='crashQuery')process.exit(23);
  setTimeout(()=>{reply(message.id,{contents:'fixture',pid:process.pid,cwd:process.cwd(),rootUri});if(options.mode==='exitAfterQuery')setTimeout(()=>process.exit(24),10);},options.mode==='slowQuery'?options.delay:0);
 }else if(message.method==='shutdown')reply(message.id,null);
 else if(message.method==='exit')process.exit(0);
}});
"#;

struct Fixture {
    directory: tempfile::TempDir,
    runtime: Arc<Runtime>,
    config: Value,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("fixture.js"), FIXTURE).unwrap();
        std::fs::write(directory.path().join("test.custom"), "sample").unwrap();
        std::fs::write(
            directory.path().join("control.json"),
            r#"{"mode":"normal"}"#,
        )
        .unwrap();
        let config = json!({"name":"fixture","command":"node","args":[directory.path().join("fixture.js")],"env":{"LSP_FIXTURE_ROOT":directory.path()},"extensionToLanguage":{".custom":"fixture"},"startupTimeout":1000,"maxRestarts":1});
        Self {
            directory,
            runtime: Arc::default(),
            config,
        }
    }
    fn cwd(&self) -> &str {
        self.directory.path().to_str().unwrap()
    }
    fn configure(&self) {
        configure_in(&self.runtime, self.cwd(), vec![self.config.clone()]).unwrap();
    }
    fn control(&self, value: Value) {
        std::fs::write(
            self.directory.path().join("control.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }
    async fn query(&self, seconds: u64) -> Result<Value> {
        execute_in(
            &self.runtime,
            json!({"filePath":"test.custom","operation":"hover","timeout_secs":seconds}),
            self.cwd(),
        )
        .await
    }
    fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.directory.path().join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
    fn spawns(&self) -> usize {
        self.events()
            .iter()
            .filter(|value| value["event"] == "spawn")
            .count()
    }
    async fn wait_event(&self, event: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self.events().iter().any(|value| value["event"] == event) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }
    async fn assert_dead(&self, pids: Vec<u64>) {
        if pids.is_empty() {
            return;
        }
        #[cfg(unix)]
        {
            // A cached Child is still owned by the pool. Reap an exited child
            // before kill(pid, 0), which also succeeds for Unix zombies.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
            loop {
                let mut live = false;
                let mut pool = self.runtime.pool.lock().await;
                for (_, server) in pool.values_mut() {
                    if server
                        .child
                        .id()
                        .is_some_and(|pid| pids.contains(&u64::from(pid)))
                    {
                        live |= server.child.try_wait().unwrap().is_none();
                    }
                }
                drop(pool);
                if !live || tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        let mut command = tokio::process::Command::new("node");
        command.args(["-e", "const p=JSON.parse(process.argv[1]),end=Date.now()+4000;const check=()=>{let live=p.some(pid=>{try{process.kill(pid,0);return true}catch{return false}});if(!live)process.exit(0);if(Date.now()>end)process.exit(1);setTimeout(check,25)};check();", &serde_json::to_string(&pids).unwrap()]);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        assert!(
            command.status().await.unwrap().success(),
            "LSP process survived: {pids:?}"
        );
    }
    fn pids(&self) -> Vec<u64> {
        self.events()
            .iter()
            .filter_map(|event| {
                if event["event"] == "spawn" {
                    event["pid"].as_u64()
                } else if event["event"] == "child" {
                    event["child"].as_u64()
                } else {
                    None
                }
            })
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(mut pool) = self.runtime.pool.try_lock() {
            pool.clear();
        }
    }
}

#[test]
fn invalid_lifecycle_configuration_is_rejected_without_replacing_valid_settings() {
    assert_eq!(lifecycle_options(&json!({})).unwrap(), (None, 3));
    let fixture = Fixture::new();
    fixture.configure();
    for (name, value) in [
        ("startupTimeout", json!(0)),
        ("startupTimeout", json!(-1)),
        ("startupTimeout", json!(1.5)),
        ("startupTimeout", json!("500")),
        ("startupTimout", json!(500)),
        ("maxRestarts", json!(-1)),
        ("maxRestarts", json!(1.5)),
        ("maxRestarts", Value::Null),
        ("restartOnCrash", json!(false)),
        ("shutdownTimeout", json!(100)),
        ("args", json!([""])),
        ("transport", json!(true)),
        ("extensionToLanguage", json!({"rs":"rust"})),
    ] {
        let mut config = fixture.config.clone();
        config[name] = value;
        assert!(
            configure_in(&fixture.runtime, fixture.cwd(), vec![config]).is_err(),
            "{name}"
        );
    }
    let root = fixture
        .directory
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let configs = fixture.runtime.configurations.lock().unwrap();
    assert_eq!(configs[&root].generation, 1);
    assert_eq!(configs[&root].servers, vec![fixture.config.clone()]);
}

#[tokio::test]
async fn startup_timeout_kills_tree_and_is_distinct_from_total_request_timeout() {
    let mut fixture = Fixture::new();
    fixture.config["startupTimeout"] = json!(500);
    fixture.configure();
    fixture.control(json!({"mode":"slowInit","delay":3000,"spawnChild":true}));
    let error = fixture.query(5).await.unwrap_err();
    assert!(
        error.to_string().contains("initialization timed out"),
        "{error:#}"
    );
    assert!(!fixture.pids().is_empty());
    fixture.assert_dead(fixture.pids()).await;
    fixture.config["startupTimeout"] = json!(5000);
    fixture.configure();
    let error = fixture.query(1).await.unwrap_err();
    assert!(
        error.to_string().contains("total timeout_secs"),
        "{error:#}"
    );
    fixture.assert_dead(fixture.pids()).await;
    fixture.config["startupTimeout"] = json!(500);
    fixture.configure();
    fixture.control(json!({"mode":"slowQuery","delay":800}));
    fixture.query(5).await.unwrap(); // startupTimeout must not bound the later query.
    shutdown_in(&fixture.runtime).await;
    fixture.assert_dead(fixture.pids()).await;
}

#[tokio::test]
async fn recovery_budget_is_bounded_and_only_changed_configuration_resets_it() {
    let mut fixture = Fixture::new();
    fixture.configure();
    fixture.control(json!({"mode":"crashInit"}));
    for _ in 0..2 {
        assert!(fixture.query(5).await.is_err());
    }
    assert_eq!(fixture.spawns(), 2);
    for _ in 0..2 {
        fixture.configure();
        assert!(fixture
            .query(5)
            .await
            .unwrap_err()
            .to_string()
            .contains("maxRestarts"));
    }
    assert_eq!(fixture.spawns(), 2);
    fixture.config["startupTimeout"] = json!(1500);
    fixture.configure();
    fixture.control(json!({"mode":"normal"}));
    fixture.query(5).await.unwrap();
    assert_eq!(fixture.spawns(), 3);
    shutdown_in(&fixture.runtime).await;
    fixture.assert_dead(fixture.pids()).await;
    let mut missing = Fixture::new();
    missing.config["command"] = json!(missing.directory.path().join("missing-lsp-executable"));
    missing.config["maxRestarts"] = json!(0);
    missing.configure();
    assert!(missing
        .query(5)
        .await
        .unwrap_err()
        .to_string()
        .contains("Cannot start"));
    assert!(missing
        .query(5)
        .await
        .unwrap_err()
        .to_string()
        .contains("maxRestarts"));
}

#[tokio::test]
async fn detected_crash_recovers_on_next_use_and_success_resets_consecutive_failures() {
    let fixture = Fixture::new();
    fixture.configure();
    fixture.control(json!({"mode":"crashQuery"}));
    assert!(fixture.query(5).await.is_err());
    fixture.control(json!({"mode":"normal"}));
    fixture.query(5).await.unwrap();
    assert_eq!(fixture.spawns(), 2);
    fixture.control(json!({"mode":"crashQuery"}));
    assert!(fixture.query(5).await.is_err());
    fixture.control(json!({"mode":"normal"}));
    fixture.query(5).await.unwrap();
    assert_eq!(fixture.spawns(), 3);
    fixture.control(json!({"mode":"exitAfterQuery"}));
    let result = fixture.query(5).await.unwrap();
    fixture
        .assert_dead(vec![result["result"]["pid"].as_u64().unwrap()])
        .await;
    fixture.control(json!({"mode":"normal"}));
    fixture.query(5).await.unwrap();
    assert_eq!(fixture.spawns(), 4); // Dead cached instances recover on next use too.
    shutdown_in(&fixture.runtime).await;
    let mut zero = Fixture::new();
    zero.config["maxRestarts"] = json!(0);
    zero.configure();
    zero.control(json!({"mode":"crashQuery"}));
    assert!(zero.query(5).await.is_err());
    zero.control(json!({"mode":"normal"}));
    assert!(zero
        .query(5)
        .await
        .unwrap_err()
        .to_string()
        .contains("maxRestarts"));
    assert_eq!(zero.spawns(), 1);
    zero.assert_dead(zero.pids()).await;
}

#[tokio::test]
async fn configuration_reload_drops_idle_and_does_not_recycle_inflight_old_generation() {
    let mut fixture = Fixture::new();
    fixture.configure();
    let first = fixture.query(5).await.unwrap();
    let pid = first["result"]["pid"].as_u64().unwrap();
    fixture.configure();
    let second = fixture.query(5).await.unwrap();
    assert_eq!(first["result"]["pid"], second["result"]["pid"]);
    fixture.config["settings"] = json!({"changed":true});
    fixture.configure();
    fixture.assert_dead(vec![pid]).await;
    fixture.control(json!({"mode":"slowQuery","delay":1000}));
    // The earlier log has hover events; wait specifically for the new process.
    let runtime = fixture.runtime.clone();
    let cwd = fixture.cwd().to_owned();
    let task = tokio::spawn(async move {
        execute_in(
            &runtime,
            json!({"filePath":"test.custom","operation":"hover","timeout_secs":5}),
            &cwd,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.spawns() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    fixture.config["settings"] = json!({"changed":"again"});
    fixture.configure();
    task.await.unwrap().unwrap();
    assert!(fixture.runtime.pool.lock().await.is_empty());
    fixture.assert_dead(fixture.pids()).await;
    fixture.control(json!({"mode":"normal"}));
    fixture.query(5).await.unwrap();
    assert_eq!(fixture.spawns(), 3);
    shutdown_in(&fixture.runtime).await;
}

#[tokio::test]
async fn cancellation_drops_initializing_process_and_its_children() {
    let mut fixture = Fixture::new();
    fixture.config["startupTimeout"] = json!(10000);
    fixture.configure();
    fixture.control(json!({"mode":"slowInit","delay":10000,"spawnChild":true}));
    let runtime = fixture.runtime.clone();
    let cwd = fixture.cwd().to_owned();
    let task = tokio::spawn(async move {
        execute_in(
            &runtime,
            json!({"filePath":"test.custom","operation":"hover","timeout_secs":20}),
            &cwd,
        )
        .await
    });
    fixture.wait_event("initialize").await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    fixture.assert_dead(fixture.pids()).await;
    assert!(fixture.runtime.pool.lock().await.is_empty());
}

#[tokio::test]
async fn configured_workspace_folder_controls_process_and_initialize_root() {
    let mut fixture = Fixture::new();
    let nested = fixture.directory.path().join("server-workspace");
    std::fs::create_dir(&nested).unwrap();
    fixture.config["workspaceFolder"] = json!("server-workspace");
    fixture.configure();
    let value = fixture.query(5).await.unwrap();
    assert_eq!(
        std::path::Path::new(value["result"]["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        nested.canonicalize().unwrap()
    );
    assert_eq!(
        reqwest::Url::parse(value["result"]["rootUri"].as_str().unwrap())
            .unwrap()
            .to_file_path()
            .unwrap()
            .canonicalize()
            .unwrap(),
        nested.canonicalize().unwrap()
    );
    shutdown_in(&fixture.runtime).await;
}
