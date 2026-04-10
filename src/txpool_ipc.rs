//! Txpool Unix socket: subscribe to `EthTxPoolEvent` batches, re-inject inserts as
//! RLP-encoded `EthTxPoolIpcTx` (same protocol as `monad-eth-txpool-ipc`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use alloy_primitives::TxHash;
use alloy_primitives::U256;
use futures::{SinkExt, StreamExt};
use monad_eth_txpool_ipc::EthTxPoolIpcClient;
use monad_eth_txpool_types::{EthTxPoolEventType, EthTxPoolIpcTx};
use tokio::sync::watch;
use tracing::{error, info, warn};

const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

pub async fn run_txpool_priority_loop(
    socket_path: PathBuf,
    priority: U256,
    mut shutdown: watch::Receiver<bool>,
) {
    if *shutdown.borrow() {
        return;
    }

    loop {
        if *shutdown.borrow() {
            return;
        }

        let connect = EthTxPoolIpcClient::new(&socket_path).await;
        let Ok((mut client, _snapshot)) = connect else {
            error!(path = %socket_path.display(), "txpool IPC connect failed; retrying");
            sleep_or_shutdown(RECONNECT_INTERVAL, &mut shutdown).await;
            continue;
        };

        info!(path = %socket_path.display(), "connected to Monad txpool IPC");
        let mut prioritized: HashSet<TxHash> = HashSet::new();

        'connection: loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return;
                    }
                }
                batch = client.next() => {
                    let Some(batch) = batch else {
                        warn!("txpool IPC stream ended; reconnecting");
                        break 'connection;
                    };
                    for ev in batch {
                        match ev.action {
                            EthTxPoolEventType::Insert { tx, .. } => {
                                let hash = ev.tx_hash;
                                if prioritized.contains(&hash) {
                                    continue;
                                }
                                let ipc = EthTxPoolIpcTx {
                                    tx,
                                    priority,
                                    extra_data: vec![],
                                };
                                if let Err(e) = client.send(ipc).await {
                                    warn!(?e, "txpool IPC send failed; reconnecting");
                                    break 'connection;
                                }
                                prioritized.insert(hash);
                            }
                            EthTxPoolEventType::Commit
                            | EthTxPoolEventType::Drop { .. }
                            | EthTxPoolEventType::Evict { .. } => {
                                prioritized.remove(&ev.tx_hash);
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn sleep_or_shutdown(duration: Duration, shutdown: &mut watch::Receiver<bool>) {
    tokio::select! {
        _ = tokio::time::sleep(duration) => {}
        _ = shutdown.changed() => {}
    }
}
