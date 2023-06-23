use dotenv::dotenv;
use futures::{lock::Mutex, Stream};
use std::{pin::Pin, sync::Arc};
use tokio::{select, sync::mpsc};
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};
use tokio_tungstenite::tungstenite::Result;
use tonic::{transport::Server, Status};

use ckt_lib::{
    booksummary::{
        orderbook_aggregator_server::{OrderbookAggregator, OrderbookAggregatorServer},
        Empty, ExchangeType, Summary, SummaryRequest, Symbols,
    },
    orderbook::Orderbook,
    symbol::{get_symbols_all, validate_symbol},
    update::get_stream,
    update::{get_updates_binance, get_updates_bitstamp},
};

#[derive(Debug)]
pub struct OrderbookSummary {
    orderbook: Arc<Mutex<Orderbook>>,
}

impl Default for OrderbookSummary {
    fn default() -> Self {
        Self {
            orderbook: Arc::new(Mutex::new(Orderbook::default())),
        }
    }
}

#[async_trait::async_trait]
impl OrderbookAggregator for OrderbookSummary {
    type WatchSummaryStream = Pin<Box<dyn Stream<Item = Result<Summary, Status>> + Send>>;

    /// Returns a list of symbols present on all exchanges.
    async fn get_symbols(
        &self,
        request: tonic::Request<Empty>,
    ) -> Result<tonic::Response<Symbols>, Status> {
        let addr = request.remote_addr().unwrap();
        tracing::info!("Got a request for symbols from {:?}", addr);
        let symbols: Symbols = get_symbols_all()
            .await
            .map_err(|_| Status::internal("Failed to get symbols"))?;
        let response = tonic::Response::new(symbols);
        Ok(response)
    }

    /// Gets an orderbook summary for a given symbol from the most recently available
    /// snapshots from teh exchanges.
    async fn get_summary(
        &self,
        request: tonic::Request<SummaryRequest>,
    ) -> Result<tonic::Response<Summary>, Status> {
        let SummaryRequest {
            symbol,
            levels,
            min_price,
            max_price,
            decimals,
        } = request.into_inner();

        validate_symbol(&symbol).await?;
        let mut ob = self.orderbook.lock().await;
        ob.reset(symbol.clone(), levels, min_price, max_price, decimals);
        ob.add_snapshots()
            .await
            .map_err(|_| Status::internal("Failed to add snapshots"))?;

        let summary = ob.get_summary();
        let response = tonic::Response::new(summary);
        Ok(response)
    }

    /// Streaming summary for a given symbol, updated for changes from all exchanges.
    async fn watch_summary(
        &self,
        request: tonic::Request<SummaryRequest>,
    ) -> Result<tonic::Response<Self::WatchSummaryStream>, Status> {
        let addr = &request.remote_addr().unwrap();
        let SummaryRequest {
            symbol,
            levels,
            min_price,
            max_price,
            decimals,
        } = request.into_inner();

        tracing::info!(
            "Received request to stream summary with {} level() for symbol {} from {}",
            levels,
            symbol,
            addr
        );
        let (tx, rx) = mpsc::unbounded_channel();

        validate_symbol(&symbol).await?;
        {
            let ob_clone = self.orderbook.clone();
            let mut ob = ob_clone.lock().await;
            ob.reset(symbol.clone(), levels, min_price, max_price, decimals);
            ob.add_snapshots()
                .await
                .map_err(|_| Status::internal("Failed to add snapshots"))?;
        }

        let mut map = get_stream(symbol)
            .await
            .map_err(|_| Status::internal("Could not get stream"))?;
        tracing::info!("Opened stream");
        let ob_clone = self.orderbook.clone();
        tokio::spawn(async move {
            let mut ob = ob_clone.lock().await;
            loop {
                select! {
                    Some((key, msg)) = map.next() => {
                        let msg = msg.map_err(|_| Status::internal("Failed to get message"))?;

                        if let Ok(msg_value) = serde_json::from_slice(&msg.into_data()) {
                            match key {
                                ExchangeType::Binance => {
                                    if let Ok(updates) = get_updates_binance(&msg_value) {
                                        ob.update(updates).await.map_err(|_| Status::internal("Failed to process binance update"))?;
                                    };
                                }
                                ExchangeType::Bitstamp => {
                                    if let Some(data) = msg_value["data"].as_object() {
                                        if data.len() > 0 {
                                            let updates =
                                                get_updates_bitstamp(data).map_err(|_| Status::internal("failed to get updates"))?;
                                                ob.update(updates).await.map_err(|_| Status::internal("Failed to process bitstamp update"))?;
                                        }
                                    };
                                }
                            }
                        };
                        if let Err(err) = tx.send(Ok(ob.get_summary())) {
                            tracing::error!("Error sending summary: {:?}", err);
                            return Err(Status::internal("Error sending summary"));
                        }
                    },
                () = tx.closed() => {
                    tracing::info!("Client closed stream");
                    return Ok(());
                }
                }
            }
        });

        let stream = UnboundedReceiverStream::new(rx);
        Ok(tonic::Response::new(
            Box::pin(stream) as Self::WatchSummaryStream
        ))
    }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let subscriber = tracing_subscriber::fmt()
        .with_line_number(true)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    dotenv().ok();
    let addr = "127.0.0.1:9001";
    tracing::info!("Server listening on {}", addr);

    let socket_addr = addr.parse()?;
    let orderbook = OrderbookSummary::default();
    Server::builder()
        .add_service(OrderbookAggregatorServer::new(orderbook))
        .serve(socket_addr)
        .await?;
    Ok(())
}
