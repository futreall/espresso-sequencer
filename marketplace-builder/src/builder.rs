use std::{num::NonZeroUsize, time::Duration};

use anyhow::Context;
use async_broadcast::{
    broadcast, Receiver as BroadcastReceiver, RecvError, Sender as BroadcastSender, TryRecvError,
};
use async_compatibility_layer::{
    art::{async_sleep, async_spawn},
    channel::{unbounded, UnboundedReceiver, UnboundedSender},
};
use async_lock::RwLock;
use async_std::sync::Arc;
use espresso_types::{
    eth_signature_key::EthKeyPair, v0_3::ChainConfig, FeeAmount, L1Client, MarketplaceVersion,
    MockSequencerVersions, NamespaceId, NodeState, Payload, SeqTypes, SequencerVersions,
    ValidatedState, V0_1,
};
use ethers::{
    core::k256::ecdsa::SigningKey,
    signers::{coins_bip39::English, MnemonicBuilder, Signer as _, Wallet},
    types::{Address, U256},
};
use hotshot::traits::BlockPayload;
use hotshot_builder_api::v0_3::builder::{
    BuildError, Error as BuilderApiError, Options as HotshotBuilderApiOptions,
};
use hotshot_events_service::{
    events::{Error as EventStreamApiError, Options as EventStreamingApiOptions},
    events_source::{EventConsumer, EventsStreamer},
};
use hotshot_types::{
    data::{fake_commitment, Leaf, ViewNumber},
    traits::{
        block_contents::{vid_commitment, GENESIS_VID_NUM_STORAGE_NODES},
        node_implementation::{ConsensusTime, NodeType, Versions},
        EncodeBytes,
    },
    utils::BuilderCommitment,
};
use marketplace_builder_core::{
    builder_state::{
        BuildBlockInfo, BuilderState, BuiltFromProposedBlock, MessageType, ResponseMessage,
    },
    service::{
        run_builder_service, BroadcastSenders, BuilderHooks, GlobalState, ProxyGlobalState,
        ReceivedTransaction,
    },
};
use sequencer::{catchup::StatePeers, L1Params, NetworkParams, SequencerApiVersion};
use surf::http::headers::ACCEPT;
use surf_disco::Client;
use tide_disco::{app, method::ReadState, App, Url};
use vbs::version::{StaticVersion, StaticVersionType};

use crate::hooks::{self, BidConfig, EspressoFallbackHooks, EspressoReserveHooks};

#[derive(Clone, Debug)]
pub struct BuilderConfig {
    pub global_state: Arc<RwLock<GlobalState<SeqTypes>>>,
    pub hotshot_events_api_url: Url,
    pub hotshot_builder_apis_url: Url,
}

pub fn build_instance_state<V: Versions>(
    chain_config: ChainConfig,
    l1_params: L1Params,
    state_peers: Vec<Url>,
) -> anyhow::Result<NodeState> {
    let l1_client = L1Client::new(l1_params.url, l1_params.events_max_block_range);

    let instance_state = NodeState::new(
        u64::MAX, // dummy node ID, only used for debugging
        chain_config,
        l1_client,
        Arc::new(StatePeers::<SequencerApiVersion>::from_urls(
            state_peers,
            Default::default(),
        )),
        V::Base::version(),
    );
    Ok(instance_state)
}

impl BuilderConfig {
    async fn start_service<H>(
        global_state: Arc<RwLock<GlobalState<SeqTypes>>>,
        senders: BroadcastSenders<SeqTypes>,
        hooks: Arc<H>,
        builder_key_pair: EthKeyPair,
        events_api_url: Url,
        builder_api_url: Url,
        api_timeout: Duration,
    ) -> anyhow::Result<()>
    where
        H: BuilderHooks<SeqTypes>,
    {
        // create the proxy global state it will server the builder apis
        let app = ProxyGlobalState::new(
            global_state.clone(),
            Arc::clone(&hooks),
            (builder_key_pair.fee_account(), builder_key_pair.clone()),
            api_timeout,
        )
        .into_app()
        .context("Failed to construct builder API app")?;

        async_spawn(async move {
            tracing::info!("Starting builder API app at {builder_api_url}");
            let res = app
                .serve(builder_api_url, MarketplaceVersion::instance())
                .await;
            tracing::error!(?res, "Builder API app exited");
        });

        // spawn the builder service
        tracing::info!("Running builder against hotshot events API at {events_api_url}",);

        let stream = marketplace_builder_core::utils::EventServiceStream::<
            SeqTypes,
            SequencerApiVersion,
        >::connect(events_api_url)
        .await?;

        async_spawn(async move {
            let res = run_builder_service::<SeqTypes>(hooks, senders, stream).await;
            tracing::error!(?res, "Builder service exited");
            if res.is_err() {
                panic!("Builder should restart.");
            }
        });

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn init(
        is_reserve: bool,
        builder_key_pair: EthKeyPair,
        bootstrapped_view: ViewNumber,
        tx_channel_capacity: NonZeroUsize,
        event_channel_capacity: NonZeroUsize,
        instance_state: NodeState,
        validated_state: ValidatedState,
        events_api_url: Url,
        builder_api_url: Url,
        api_timeout: Duration,
        buffered_view_num_count: usize,
        maximize_txns_count_timeout_duration: Duration,
        base_fee: FeeAmount,
        bid_config: Option<BidConfig>,
        solver_api_url: Url,
    ) -> anyhow::Result<Self> {
        println!("here start init");
        tracing::info!(
            address = %builder_key_pair.fee_account(),
            ?bootstrapped_view,
            %tx_channel_capacity,
            %event_channel_capacity,
            ?api_timeout,
            buffered_view_num_count,
            ?maximize_txns_count_timeout_duration,
            "initializing builder",
        );

        let (mut senders, receivers) =
            marketplace_builder_core::service::broadcast_channels(event_channel_capacity.get());

        senders.transactions.set_capacity(tx_channel_capacity.get());

        // builder api request channel
        let (req_sender, req_receiver) =
            broadcast::<MessageType<SeqTypes>>(event_channel_capacity.get());

        let (genesis_payload, genesis_ns_table) =
            Payload::from_transactions([], &validated_state, &instance_state)
                .await
                .expect("genesis payload construction failed");
        println!("here genesis");

        let builder_commitment = genesis_payload.builder_commitment(&genesis_ns_table);

        let vid_commitment = {
            let payload_bytes = genesis_payload.encode();
            vid_commitment(&payload_bytes, GENESIS_VID_NUM_STORAGE_NODES)
        };

        // create the global state
        let global_state: GlobalState<SeqTypes> = GlobalState::<SeqTypes>::new(
            req_sender,
            senders.transactions.clone(),
            vid_commitment,
            bootstrapped_view,
        );
        println!("here global state");

        let global_state = Arc::new(RwLock::new(global_state));

        let builder_state = BuilderState::<SeqTypes>::new(
            BuiltFromProposedBlock {
                view_number: bootstrapped_view,
                vid_commitment,
                leaf_commit: fake_commitment(),
                builder_commitment,
            },
            &receivers,
            req_receiver,
            Vec::new() /* tx_queue */,
            Arc::clone(&global_state),
            maximize_txns_count_timeout_duration,
            base_fee
                .as_u64()
                .context("the base fee exceeds the maximum amount that a builder can pay (defined by u64::MAX)")?,
            Arc::new(instance_state),
            Duration::from_secs(60),
            Arc::new(validated_state),
        );
        println!("here builder state");

        // Start builder event loop
        builder_state.event_loop();

        if is_reserve {
            let bid_config = bid_config.expect("Missing bid config for the reserve builder.");
            let hooks = Arc::new(hooks::EspressoReserveHooks {
                namespaces: bid_config.namespaces.into_iter().collect(),
                solver_api_url,
                builder_api_base_url: builder_api_url.clone(),
                bid_key_pair: builder_key_pair.clone(),
                bid_amount: bid_config.amount,
            });
            Self::start_service(
                Arc::clone(&global_state),
                senders,
                hooks,
                builder_key_pair,
                events_api_url.clone(),
                builder_api_url.clone(),
                api_timeout,
            )
            .await?;
        } else {
            let hooks = Arc::new(hooks::EspressoFallbackHooks {
                solver_api_url,
                namespaces_to_skip: RwLock::new(None),
            });
            Self::start_service(
                Arc::clone(&global_state),
                senders,
                hooks,
                builder_key_pair,
                events_api_url.clone(),
                builder_api_url.clone(),
                api_timeout,
            )
            .await?;
        }
        println!("here run_non_permissioned_standalone_builder_service");

        tracing::info!("Builder init finished");

        Ok(Self {
            global_state,
            hotshot_events_api_url: events_api_url,
            hotshot_builder_apis_url: builder_api_url,
        })
    }
}

#[cfg(test)]
mod test {
    use std::{
        str::FromStr,
        time::{Duration, Instant},
    };

    use async_compatibility_layer::{
        art::{async_sleep, async_spawn},
        logging::{setup_backtrace, setup_logging},
    };
    use async_lock::RwLock;
    use async_std::{prelude::FutureExt, stream::StreamExt, task};
    use committable::Commitment;
    use committable::Committable;
    use espresso_types::{
        mock::MockStateCatchup,
        v0_3::{RollupRegistration, RollupRegistrationBody},
        FeeAccount, MarketplaceVersion, NamespaceId, PubKey, SeqTypes, SequencerVersions,
        Transaction,
    };
    use ethers::utils::Anvil;
    use hotshot::types::{BLSPrivKey, Event, EventType};
    use hotshot_builder_api::v0_3::builder::BuildError;
    use hotshot_events_service::{
        events::{Error as EventStreamApiError, Options as EventStreamingApiOptions},
        events_source::{EventConsumer, EventsStreamer},
    };
    use hotshot_query_service::{availability::LeafQueryData, VidCommitment};
    use hotshot_types::{
        bundle::Bundle,
        light_client::StateKeyPair,
        signature_key::BLSPubKey,
        traits::{
            block_contents::{BlockPayload, GENESIS_VID_NUM_STORAGE_NODES},
            node_implementation::{NodeType, Versions},
            signature_key::{BuilderSignatureKey, SignatureKey},
        },
    };
    use marketplace_builder_core::{
        builder_state::{self, RequestMessage, TransactionSource},
        service::run_builder_service,
        utils::BuilderStateId,
    };
    use marketplace_solver::{testing::MockSolver, SolverError};
    use portpicker::pick_unused_port;
    use sequencer::{
        api::test_helpers::TestNetworkConfigBuilder,
        persistence::no_storage::{self, NoStorage},
        testing::TestConfigBuilder,
        SequencerApiVersion,
    };
    use sequencer::{
        api::{fs::DataSource, options::HotshotEvents, test_helpers::TestNetwork, Options},
        persistence,
    };
    use sequencer_utils::test_utils::setup_test;
    use surf_disco::Client;
    use tempfile::TempDir;
    use tide_disco::error::ServerError;
    use vbs::version::StaticVersion;

    use super::*;

    #[async_std::test]
    async fn test_marketplace_fallback_builder() {
        setup_test();

        let query_port = pick_unused_port().expect("No ports free");
        let query_api_url: Url = format!("http://localhost:{query_port}").parse().unwrap();

        let event_port = pick_unused_port().expect("No ports free");
        let event_service_url: Url = format!("http://localhost:{event_port}").parse().unwrap();

        let builder_port = pick_unused_port().expect("No ports free");
        let builder_api_url: Url = format!("http://localhost:{builder_port}").parse().unwrap();

        // Set up and start the network
        let anvil = Anvil::new().spawn();
        let l1 = anvil.endpoint().parse().unwrap();
        let network_config = TestConfigBuilder::default().l1_url(l1).build();

        let tmpdir = TempDir::new().unwrap();
        println!("here after tmpdir");

        let config = TestNetworkConfigBuilder::default()
            .api_config(
                Options::with_port(query_port)
                    .submit(Default::default())
                    .query_fs(
                        Default::default(),
                        persistence::fs::Options::new(tmpdir.path().to_owned()),
                    )
                    .hotshot_events(HotshotEvents {
                        events_service_port: event_port,
                    }),
            )
            .network_config(network_config)
            .build();
        let _network = TestNetwork::new(config, MockSequencerVersions::new()).await;

        let mock_solver = MockSolver::init().await;
        let solver_api = mock_solver.solver_api();
        println!("here after solver api");
        let client = surf_disco::Client::<SolverError, MarketplaceVersion>::new(solver_api.clone());

        // Create a list of signature keys for rollup registration data
        let mut signature_keys = Vec::new();

        for _ in 0..10 {
            let private_key =
                <BLSPubKey as SignatureKey>::PrivateKey::generate(&mut rand::thread_rng());
            signature_keys.push(BLSPubKey::from_private(&private_key))
        }

        let private_key =
            <BLSPubKey as SignatureKey>::PrivateKey::generate(&mut rand::thread_rng());
        let signature_key = BLSPubKey::from_private(&private_key);

        signature_keys.push(signature_key);

        // Initialize a rollup registration with namespace id = 10
        let reg_ns_1_body = RollupRegistrationBody {
            namespace_id: 10_u64.into(),
            reserve_url: Some(Url::from_str("http://localhost").unwrap()),
            reserve_price: 200.into(),
            active: true,
            signature_keys: signature_keys.clone(),
            text: "test".to_string(),
            signature_key,
        };

        // Sign the registration body
        let reg_signature = <SeqTypes as NodeType>::SignatureKey::sign(
            &private_key,
            reg_ns_1_body.commit().as_ref(),
        )
        .expect("failed to sign");

        let reg_ns_1 = RollupRegistration {
            body: reg_ns_1_body.clone(),
            signature: reg_signature,
        };

        // registering a rollup
        let result: RollupRegistration = client
            .post("register_rollup")
            .body_json(&reg_ns_1)
            .unwrap()
            .send()
            .await
            .unwrap();
        let result: Vec<RollupRegistration> =
            client.get("rollup_registrations").send().await.unwrap();
        assert_eq!(result, vec![reg_ns_1]);

        // Start the builder
        let init = BuilderConfig::init(
            false,
            FeeAccount::test_key_pair(),
            ViewNumber::genesis(),
            NonZeroUsize::new(1024).unwrap(),
            NonZeroUsize::new(1024).unwrap(),
            NodeState::default(),
            ValidatedState::default(),
            event_service_url.clone(),
            builder_api_url.clone(),
            Duration::from_secs(2),
            5,
            Duration::from_secs(2),
            FeeAmount::from(10),
            None,
            solver_api,
        );
        println!("here after init");
        let builder_config = init.await.unwrap();

        // Wait for at least one empty block to be sequenced (after consensus starts VID).
        let sequencer_client: Client<ServerError, SequencerApiVersion> = Client::new(query_api_url);
        sequencer_client.connect(None).await;
        sequencer_client
            .socket("availability/stream/leaves/0")
            .subscribe::<LeafQueryData<SeqTypes>>()
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .unwrap();

        //  Connect to builder
        let builder_client: Client<ServerError, MarketplaceVersion> =
            Client::new(builder_api_url.clone());
        builder_client.connect(None).await;

        let txn_submission_client: Client<ServerError, SequencerApiVersion> =
            Client::new(builder_api_url.clone());
        txn_submission_client.connect(None).await;

        // Test submitting transactions
        let transactions = vec![
            Transaction::new(10u32.into(), vec![1, 1, 1, 1]),
            Transaction::new(20u32.into(), vec![1, 1, 1, 2]),
        ];
        txn_submission_client
            .post::<Vec<Commitment<Transaction>>>("txn_submit/batch")
            .body_json(&transactions)
            .unwrap()
            .send()
            .await
            .unwrap();

        let events_service_client = Client::<
            hotshot_events_service::events::Error,
            SequencerApiVersion,
        >::new(event_service_url.clone());
        events_service_client.connect(None).await;

        let mut subscribed_events = events_service_client
            .socket("hotshot-events/events")
            .subscribe::<Event<SeqTypes>>()
            .await
            .unwrap();

        task::sleep(std::time::Duration::from_millis(100)).await;

        let start = Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(10) {
                panic!("Didn't get a quorum proposal in 10 seconds");
            }

            let event = subscribed_events.next().await.unwrap().unwrap();
            if let EventType::QuorumProposal { proposal, .. } = event.event {
                let parent_view_number = *proposal.data.view_number;
                println!("here parent_view_number {:?}", parent_view_number);
                let parent_commitment =
                    Leaf::from_quorum_proposal(&proposal.data).payload_commitment();
                let bundle = builder_client
                    .get::<Bundle<SeqTypes>>(
                        format!(
                            "block_info/bundle/{parent_view_number}/{parent_commitment}/{}",
                            parent_view_number + 1
                        )
                        .as_str(),
                    )
                    .send()
                    .await
                    .unwrap();
                assert_eq!(bundle.transactions, transactions);
                println!("here transactions {:?}", transactions);
                break;
            }
        }
    }
}
