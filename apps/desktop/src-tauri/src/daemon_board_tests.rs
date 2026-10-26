use super::*;
use codypendent_protocol::{Actor, DaemonInstanceId, EventBody, ServerHello, PROTOCOL_V1};
use std::sync::Mutex as StdMutex;
use tokio::net::{UnixListener, UnixStream};

#[derive(Default)]
struct BoardCollector(StdMutex<Vec<DaemonFrame>>);

impl FrameSink for BoardCollector {
    fn emit(&self, frame: DaemonFrame) {
        self.0.lock().unwrap().push(frame);
    }
}

type Commands = Arc<StdMutex<Vec<(usize, CommandBody)>>>;

fn task(scope: &str, status: &str) -> BlackboardItemView {
    serde_json::from_value(serde_json::json!({
        "id": "task-1", "workflow_run_id": scope, "kind": "task", "author": {"role":"human"},
        "payload": {"title":"A repository task"}, "evidence": [], "revision": 1, "status": status,
    }))
    .unwrap()
}

async fn serve_board_connection(
    mut stream: UnixStream,
    index: usize,
    commands: Commands,
    scope: String,
) {
    let internal = SessionId::new();
    while let Ok(Some(request)) = read_envelope(&mut stream).await {
        let mut reply = match &request.payload {
            Payload::ClientHello(_) => Envelope::reply_to(
                &request,
                Payload::ServerHello(ServerHello {
                    selected_protocol: PROTOCOL_V1,
                    daemon_version: "board-test".into(),
                    daemon_instance: DaemonInstanceId::new(),
                    heartbeat_interval_ms: 15_000,
                    resume_token: None,
                    build_id: "board-test".into(),
                }),
            ),
            Payload::Command(command) => {
                commands.lock().unwrap().push((index, command.body.clone()));
                let payload = match &command.body {
                    CommandBody::CreateSession { internal: true, .. }
                    | CommandBody::CloseSession { .. } => Payload::CommandAccepted {
                        command_id: command.command_id,
                        sequence: None,
                        created_run: None,
                    },
                    CommandBody::AttachSession { .. } => Payload::Catchup {
                        catchup: Catchup::Events {
                            from: 0,
                            through: 1,
                            events: vec![SessionEvent {
                                sequence: 1,
                                occurred_at: chrono::Utc::now(),
                                causation_id: None,
                                correlation_id: None,
                                actor: Actor::System,
                                body: EventBody::SessionCreated {
                                    title: "Board service".into(),
                                },
                            }],
                        },
                    },
                    CommandBody::ReadBlackboard { .. } => Payload::BlackboardItems {
                        command_id: command.command_id,
                        items: vec![],
                    },
                    CommandBody::PostBlackboardItem { .. } => Payload::BlackboardItemApplied {
                        command_id: command.command_id,
                        item: task(&scope, "todo"),
                    },
                    CommandBody::UpdateBlackboardItem { .. } => Payload::BlackboardItemApplied {
                        command_id: command.command_id,
                        item: task(&scope, "doing"),
                    },
                    other => panic!("unexpected board command: {other:?}"),
                };
                Envelope::reply_to(&request, payload)
            }
            _ => continue,
        };
        reply.session_id = Some(internal);
        write_envelope(&mut stream, &reply).await.unwrap();
        if matches!(&request.payload, Payload::Command(command) if matches!(&command.body, CommandBody::ReadBlackboard {..}))
        {
            let live = Envelope::request(
                request.client_id,
                Payload::BlackboardPosted(task(&scope, "todo")),
            );
            write_envelope(&mut stream, &live).await.unwrap();
        }
    }
}

#[tokio::test]
async fn task_board_uses_one_private_connection_without_creating_a_chat() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("board.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let commands: Commands = Arc::default();
    let observed = Arc::clone(&commands);
    let repository = directory
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let scope = board_scope_id(&repository);
    tokio::spawn(async move {
        for index in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(serve_board_connection(
                stream,
                index,
                Arc::clone(&observed),
                scope.clone(),
            ));
        }
    });
    let sink = Arc::new(BoardCollector::default());
    let (client, _) = DaemonClient::connect(&socket, Some(repository.clone()), Arc::clone(&sink))
        .await
        .unwrap();
    let board = tokio::time::timeout(Duration::from_secs(2), client.watch_board(&sink))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(board.repository, repository);
    assert!(client.attached.lock().await.is_none());
    tokio::time::timeout(Duration::from_secs(1), async {
        while sink.0.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(sink
        .0
        .lock()
        .unwrap()
        .iter()
        .all(|frame| matches!(frame, DaemonFrame::BlackboardPosted { .. })));
    client.watch_board(&sink).await.unwrap();
    assert_eq!(
        client
            .create_board_card("A repository task".into())
            .await
            .unwrap()
            .status
            .as_deref(),
        Some("todo")
    );
    assert_eq!(
        client
            .move_board_card("task-1".into(), "doing".into())
            .await
            .unwrap()
            .status
            .as_deref(),
        Some("doing")
    );
    let visible = SessionId::new();
    client.attach(visible, &sink).await.unwrap();
    client.watch_board(&sink).await.unwrap();
    assert_eq!(*client.attached.lock().await, Some(visible));
    client.shutdown().await;
    let commands = commands.lock().unwrap();
    assert_eq!(
        commands
            .iter()
            .filter(|(_, command)| matches!(
                command,
                CommandBody::CreateSession { internal: true, .. }
            ))
            .count(),
        1
    );
    assert!(commands
        .iter()
        .filter(|(_, command)| matches!(
            command,
            CommandBody::ReadBlackboard { .. }
                | CommandBody::PostBlackboardItem { .. }
                | CommandBody::UpdateBlackboardItem { .. }
        ))
        .all(|(connection, _)| *connection == 1));
    assert_eq!(
        commands
            .iter()
            .filter(|(_, command)| matches!(command, CommandBody::CloseSession { .. }))
            .count(),
        1
    );
}
