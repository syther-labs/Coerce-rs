cargo run --bin sharded-chat-server -- --node_id 3 --remote_listen_addr localhost:33101 --websocket_listen_addr  localhost:33102 --cluster_api_listen_addr  0.0.0.0:33103 --remote_seed_addr localhost:31101 --log_level DEBUG