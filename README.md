# Fish

Using an event based interaction pattern for callback communication is often praised as ideal, but,
for many cases we've found this results in complexity, bugs, and unnecessary work. `Fish`
provides imperative non-blocking runtime interaction for callback and webhook based APIs.

Let's contrive an example to see how this works: imagine you are the developer of some plugin for a delivery app.

The app will notify you (... via a webhook) when a new delivery region opens.
There is an endpoint that allows you to register a webhook for new orders
in that region.

Whenever an order is received, you can send a request to accept it. The delivery app
will then send you a callback if your request is granted. Oh, I forgot to mention,
you are a broker, so, whenever the order is received, you need to pass it along to
your fulfillment client, who will likewise send a callback if they desire to fulfill the order.

Along each step, you have domain-specific logic and state that is often used throughout the
entire lifecycle.

In `Fish`, this interaction looks something like:

```rust
// Step 1: Register webhook for new delivery regions
let orders = server.spawn();

app.send("region", RegisterRegion {
    webhook_url: orders.url(),
    region_id
}).await;

// orders is a Stream. You could concatenate multiple regions' orders together,
// or maybe handle them separately
while let Ok(order) = orders.next().await {
   // Step 2: Let's see if our fulfillment partner is interested in the order
   let fulfillment = server.spawn();

    partner.send("order", NotifyOrder {
        callback: fulfillment.url(),
        ..order
    }).await;

    // The partner will send us back a POST request if they want to fulfill the order,
    // and do-nothing otherwise. We have a time limit to adhere to, so we'll give them
    // 5 seconds to respond
   if let Ok(_) = timeout(Duration::from_secs(5), fulfillment.next().await) {
     // Step 3: OK, we're set, lets let the app know we are interested!
     let granted = server.spawn();
     app.send("order", AcceptOrder {
          callback_url: url,
          ..
     }).await;

     // granted.next() and so on!
   }
}
```

Ok, we could've drawn this out a lot further. And sorely missing is the domain-specific logic,
state updates, caching, and so on that happens during this orchestration. We've found doing
this in an event-handler-based manner takes an enormous effort.

What about you? Have you found this to be challenging? Any suggestions?

License: MIT OR Apache-2.0

To think about:

How does this pattern mesh well with CI/CD, hardware faults, and so on? For example,
let's say there are 10 callbacks outstanding. If the server is restarted, the receivers
for that will cease. In an "event-based" system, the server would boot back up and
everything would be hunky-dory, the 3rd party might see a 504 while down and do a retry.

There is unrecoverable state.
