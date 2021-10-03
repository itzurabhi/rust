use jsonrpc_derive::rpc;
use serde::{Serialize, Deserialize};

use std::sync::Arc;
use jsonrpc_core::Result;
use jsonrpc_pubsub::{typed::Subscriber, SubscriptionId, Session, PubSubHandler};

#[rpc]
pub trait Rpc<T> {
	type Metadata;

	/// Hello subscription
	#[pubsub(subscription = "hello", subscribe, name = "hello_subscribe", alias("hello_sub"))]
	fn subscribe(&self, _: Self::Metadata, _: Subscriber<T>);

	/// Unsubscribe from hello subscription.
	#[pubsub(subscription = "hello", unsubscribe, name = "hello_unsubscribe")]
	fn unsubscribe(&self, a: Option<Self::Metadata>, b: SubscriptionId) -> Result<bool>;
}

#[derive(Serialize, Deserialize)]
struct SerializeAndDeserialize {
	foo: String,
}

struct RpcImpl;
impl Rpc<SerializeAndDeserialize> for RpcImpl {
	type Metadata = Arc<Session>;

	fn subscribe(&self, _: Self::Metadata, _: Subscriber<SerializeAndDeserialize>) {
		unimplemented!();
	}

	fn unsubscribe(&self, _: Option<Self::Metadata>, _: SubscriptionId) -> Result<bool> {
		unimplemented!();
	}
}

fn main() {
	let mut io = PubSubHandler::default();
	let rpc = RpcImpl;
	io.extend_with(rpc.to_delegate());
}
