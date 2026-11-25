//! The Agent Client Protocol (https://agentclientprotocol.com), Kuana's side:
//! the editor end of a connection to a coding agent (Claude, Codex, Gemini
//! CLI, GitHub Copilot, OpenCode… and Kuana's own agent, in-process). One
//! JSON-RPC object per line over the agent's stdio ([`rpc`]); typed with
//! `agent-client-protocol-schema` ([`client`]). GPUI-free: the session views
//! sit on top.

pub mod client;
pub mod install;
pub mod registry;
pub mod rpc;

/// ACP v1's message types.
pub use agent_client_protocol_schema::v1 as schema;
pub use client::{Agent, FromAgent, Launch, Responder};
pub use rpc::RpcError;

#[cfg(test)]
pub(crate) mod testing {
    //! A fake agent on an in-process pipe: it answers `initialize` itself and
    //! the rest through the test's handler, on a thread of its own.

    use futures::StreamExt as _;
    use serde_json::{Value, json};

    use super::rpc::{Connection, Incoming, RpcError};
    use super::Agent;
    use super::client::Events;

    pub struct FakeAgent {
        client_side: Option<(std::io::PipeReader, std::io::PipeWriter)>,
    }

    impl FakeAgent {
        pub fn start(handler: impl Fn(&Connection, &str, &Value) -> Result<Value, RpcError> + Send + Sync + 'static) -> FakeAgent {
            let (client_read, agent_write) = std::io::pipe().unwrap();
            let (agent_read, client_write) = std::io::pipe().unwrap();
            let (agent, mut incoming) = Connection::start(agent_read, agent_write);
            let handler = std::sync::Arc::new(handler);
            std::thread::spawn(move || {
                futures::executor::block_on(async {
                    while let Some(message) = incoming.next().await {
                        let Incoming::Request { id, method, params } = message else { continue };
                        if method == "initialize" {
                            agent.respond(id, Ok(json!({ "protocolVersion": 1, "agentInfo": { "name": "fake", "title": "Fake Agent", "version": "1" } })));
                            continue;
                        }
                        // Each request on a thread, so the handler can wait
                        // on Kuana's answers to its own requests.
                        let (agent, handler) = (agent.clone(), handler.clone());
                        std::thread::spawn(move || {
                            let result = handler(&agent, &method, &params);
                            agent.respond(id, result);
                        });
                    }
                })
            });
            FakeAgent { client_side: Some((client_read, client_write)) }
        }

        pub async fn connect(mut self) -> (Agent, Events) {
            let (read, write) = self.client_side.take().unwrap();
            Agent::connect(read, write).await.unwrap()
        }
    }
}
