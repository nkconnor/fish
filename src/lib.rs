//! Using an event based interaction pattern for callback communication is often praised as ideal, but,
//! for many cases we've found this results in complexity, bugs, and unnecessary work. `Fish`
//! provides imperative non-blocking runtime interaction for callback and webhook based APIs.
//!
//! Let's contrive an example to see how this works: imagine you are the developer of some plugin for a delivery app.
//!
//! The app will notify you (... via a webhook) when a new delivery region opens.
//! There is an endpoint that allows you to register a webhook for new orders
//! in that region.
//!
//! Whenever an order is received, you can send a request to accept it. The delivery app
//! will then send you a callback if your request is granted. Oh, I forgot to mention,
//! you are a broker, so, whenever the order is received, you need to pass it along to
//! your fulfillment client, who will likewise send a callback if they desire to fulfill the order.
//!
//! Along each step, you have domain-specific logic and state that is often used throughout the
//! entire lifecycle.
//!
//! In `Fish`, this interaction looks something like:
//!
//! ```rust,ignore
//! // Step 1: Register webhook for new delivery regions
//! let orders = server.spawn();
//!
//! app.send("region", RegisterRegion {
//!     webhook_url: orders.url(),
//!     region_id
//! }).await;
//!
//! // orders is a Stream. You could concatenate multiple regions' orders together,
//! // or maybe handle them separately
//! while let Ok(order) = orders.next().await {
//!    // Step 2: Let's see if our fulfillment partner is interested in the order
//!    let fulfillment = server.spawn();
//!
//!     partner.send("order", NotifyOrder {
//!         callback: fulfillment.url(),
//!         ..order
//!     }).await;
//!     
//!     // The partner will send us back a POST request if they want to fulfill the order,
//!     // and do-nothing otherwise. We have a time limit to adhere to, so we'll give them
//!     // 5 seconds to respond
//!    if let Ok(_) = timeout(Duration::from_secs(5), fulfillment.next().await) {
//!      // Step 3: OK, we're set, lets let the app know we are interested!
//!      let granted = server.spawn();
//!      app.send("order", AcceptOrder {
//!           callback_url: url,
//!           ..
//!      }).await;
//!
//!      // granted.next() and so on!
//!    }
//! }
//! ```
//!
//! Ok, we could've drawn this out a lot further. And sorely missing is the domain-specific logic,
//! state updates, caching, and so on that happens during this orchestration. We've found doing
//! this in an event-handler-based manner takes an enormous effort.
//!
//! What about you? Have you found this to be challenging? Any suggestions?
//!
//!
//! Feedback, contributions, and bug reports welcome.
use futures::Stream;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use url::Url;
use uuid::Uuid;

use async_channel::{unbounded, Receiver, Sender, TryRecvError};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response};
use thiserror::Error;

const QUERY_PARAM_NAME: &'static str = "_async_webhook_id";

#[derive(Debug, Error)]
enum Error {
    #[error("Webhook receiver expected a unique UID but none was found")]
    WebhookMissingUid,
}

/// Responsible for negotiating outgoing requests and incoming callbacks; resolves
/// the Webhook future with received values
struct Registry {
    /// This channel will be notified when a Webhook is dropped
    /// to ensure it is removed
    drop: Sender<Uuid>,
    /// Outstanding requests pending a callback
    requests: Arc<Mutex<HashMap<Uuid, (Arc<Mutex<Option<Waker>>>, Sender<Request<Body>>)>>>,
}

impl Clone for Registry {
    fn clone(&self) -> Self {
        Registry {
            drop: self.drop.clone(),
            requests: Arc::clone(&self.requests),
        }
    }
}

impl Registry {
    /// Create a new Registry
    fn new() -> Self {
        let (drop, mut handle_drop) = unbounded::<Uuid>();

        let requests = Arc::new(Mutex::new(HashMap::new()));

        {
            let requests = requests.clone();

            use futures::stream::StreamExt;
            tokio::task::spawn(async move {
                while let Some(id) = handle_drop.next().await {
                    requests.lock().unwrap().remove(&id);
                }
            });
        }

        Self { drop, requests }
    }

    fn register(&self, uid: Uuid) -> (Arc<Mutex<Option<Waker>>>, Receiver<Request<Body>>) {
        let (sender, receiver) = unbounded();
        let mut g = self.requests.lock().unwrap();
        let waker = Arc::new(Mutex::new(None));
        g.insert(uid, (waker.clone(), sender));
        (waker, receiver)
    }

    fn notify(&self, uid: Uuid, t: Request<Body>) {
        let mut g = self.requests.lock().unwrap();
        if let Some((waker, sender)) = g.remove(&uid) {
            // Dispatch the request to the async Webhook
            // Can we be certain that the opposite side is not closed?
            sender.try_send(t).expect(
                "Webhook couldn't have been
                droped, because, otherwise, requests would have been locked 
                and the uid removed!",
            );

            // We put the request on the channel, time to wake up the pending Webhook: Future
            waker.lock().unwrap().as_ref().map(|waker| {
                waker.wake_by_ref();
            });
        }
    }
}

struct WebhookInner {
    url: Url,
    uid: Uuid,
    value: Receiver<Request<Body>>,
    dropper: Sender<Uuid>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl Drop for WebhookInner {
    fn drop(&mut self) {
        self.dropper
            .try_send(self.uid.clone())
            // can we handle this more appropriately
            .expect("Registry should have an open channel to receive dropped requests");
    }
}

pub struct Webhook {
    inner: WebhookInner,
}

impl Webhook {
    /// Return
    pub fn url(&self) -> &Url {
        &self.inner.url
    }
}

impl Stream for Webhook {
    type Item = Request<Body>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);

        // Set the waker. This will wake whenever a matching ID
        // is sent to the associated Server
        *this.inner.waker.lock().unwrap() = Some(_cx.waker().clone());

        match this.inner.value.try_recv() {
            // ..
            Ok(l) => Poll::Ready(Some(l)),
            // Nothing yet!
            Err(TryRecvError::Empty) => Poll::Pending,
            // If the server drops
            Err(TryRecvError::Closed) => Poll::Ready(None),
        }
    }
}

/// A fully kitted-callback handling server that can handle async communication
/// over HTTP-Webhooks
#[derive(Clone)]
pub struct Server {
    inner: Arc<Inner>,
}

// This is so that cloned servers don't trigger the drop condition. Is it a common pattern?
struct Inner {
    endpoint: url::Url,
    registry: Registry,
    stop: Sender<()>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.try_send(()).ok();
    }
}

impl Server {
    /// Create a new callback server on the provided address
    ///
    /// # Example
    ///
    /// ```ignore
    /// let fish = Server::start("127.0.0.1:3306");
    ///
    /// let hook = fish.spawn();
    ///
    /// let _ = async {
    ///     reqwest::get(format!("http://some.api/?callback={}", hook.url())).await
    /// };
    ///
    /// // don't forget to spin up the server!
    /// server.await
    pub fn start<A: Into<SocketAddr>>(addr: A) -> Self {
        let addr = addr.into();
        let endpoint = Url::parse(format!("http://{}", addr).as_str()).unwrap();
        Self::with_endpoint(addr, endpoint)
    }

    /// Create a callback server but with a proxy URL that will forward traffic
    /// there. E.g. you could use `ngrok` to tunnel a public Url to your
    /// development server's `fish::Server`
    pub fn start_with_proxy<A: Into<SocketAddr>>(addr: A, proxy: Url) -> Self {
        Self::with_endpoint(addr.into(), proxy)
    }

    fn with_endpoint(addr: SocketAddr, endpoint: Url) -> Self {
        let registry = Registry::new();

        type BoxError = Box<dyn std::error::Error + Send + Sync>;

        async fn handle_request(
            registry: Registry,
            request: Request<Body>,
        ) -> Result<Response<Body>, BoxError> {
            let registry = registry.clone();
            // println!("Received request at {:?}", request.uri());
            // this is a relative URL. Quick hack to make this parse to extract the query
            // parameter --

            let url = format!("http://localhost{}", request.uri());

            let url = Url::parse(url.as_str()).expect("hyper::Uri is url::Url parsable");

            let uid = url
                .query_pairs()
                // the webhook should contain our outgoing query parameter
                // with a UID
                .find(|(k, _)| k.as_ref() == QUERY_PARAM_NAME)
                .map(|(_, uid)| {
                    // that UID should be a UUID
                    Uuid::parse_str(uid.as_ref()).map_err(|_| Error::WebhookMissingUid)
                })
                .ok_or_else(|| Error::WebhookMissingUid)??;

            registry.notify(uid, request);

            Ok(Response::new(Body::from("Hello World")))
        }

        let keep_registry = registry.clone();

        let make_service = make_service_fn(move |_conn| {
            let registry = registry.clone();
            async move {
                Ok::<_, Error>(service_fn(move |request| {
                    let registry = registry.clone();
                    handle_request(registry, request)
                }))
            }
        });

        let (stop, on_stop) = async_channel::bounded::<()>(16);

        // Then bind and serve...
        let server = hyper::server::Server::bind(&addr)
            .serve(make_service)
            .with_graceful_shutdown(async move {
                on_stop.recv().await.ok();
            });

        // Lost to sea
        tokio::spawn(async { server.await });

        Server {
            inner: Arc::new(Inner {
                registry: keep_registry,
                stop,
                endpoint,
            }),
        }
    }

    /// Spawn a webhook
    ///
    /// **Example**
    ///
    /// ```rust,ignore
    /// let server = Server::new(([127, 0, 0, 1], 3031));
    /// let webhook = server.spawn();
    /// // make your call
    /// // e.g. http_client.body(webhook.url()).send().await
    /// webhook.await
    /// ```
    pub fn spawn(&self) -> Webhook {
        let uid = Uuid::new_v4();

        let url = {
            let mut url = self.inner.endpoint.clone();
            url.query_pairs_mut()
                .append_pair(QUERY_PARAM_NAME, uid.to_string().as_str());
            url
        };

        let (waker, value) = self.inner.registry.register(uid.clone());

        let dropper = self.inner.registry.drop.clone();

        Webhook {
            inner: WebhookInner {
                url,
                uid,
                value,
                dropper,
                waker,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Server, Webhook};
    use futures::{StreamExt, TryStreamExt};
    use serde::{Deserialize, Serialize};

    #[tokio::test]
    async fn test_callback_pattern() {
        // Launches a dummy API and callback server
        // and tests that the callback is received and equals what was
        // expected

        use serde::{Deserialize, Serialize};

        #[derive(Serialize, Deserialize)]
        struct Request {
            message: String,
            callback: String,
        }

        #[derive(Serialize, Deserialize)]
        struct Response {
            message: String,
        }

        eprintln!("Spawning API Server");
        // The API Server gets hit and then sends a HTTP request to the URL
        // provided in the POST body `Request.callback`
        let api_server = {
            use warp::Filter;
            let api_route =
                warp::any()
                    .and(warp::body::json::<Request>())
                    .and_then(|req: Request| async {
                        let client = reqwest::Client::new();

                        eprintln!(
                            "API received message, sending callback to: {}",
                            req.callback.as_str()
                        );

                        match client
                            .post(req.callback.as_str())
                            .json(&Response {
                                message: req.message,
                            })
                            .send()
                            .await
                        {
                            Ok(r) => {
                                eprintln!("Webhook server responded with status: {}", r.status());
                                Ok(warp::reply())
                            }
                            Err(_) => Err(warp::reject::reject()),
                        }
                    });

            tokio::spawn(warp::serve(api_route).run(([127, 0, 0, 1], 3032)))
        };

        eprintln!("Starting callback server");
        // Launch fish server on 3031
        let server: Server = Server::start(([127, 0, 0, 1], 3031));

        // Run the tests! woot
        eprintln!("Sending API request");
        let mut webhook: Webhook = server.spawn();

        let client = reqwest::Client::new();
        client
            .post("http://localhost:3032")
            .json(&Request {
                callback: webhook.url().to_string(),
                message: "hey".to_string(),
            })
            .send()
            .await;

        let res: Option<Response> = if let Some(response) = webhook.next().await {
            let bytes = response
                .into_body()
                .try_fold(Vec::new(), |mut data, chunk| async move {
                    data.extend_from_slice(&chunk);
                    Ok(data)
                })
                .await
                .unwrap();

            serde_json::from_slice(bytes.as_slice()).unwrap()
        } else {
            None
        };

        assert_eq!(res.unwrap().message.as_str(), "hey");
    }
}
