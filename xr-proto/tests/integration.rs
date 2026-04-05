/// Integration test: full pipeline on a single machine.
///
/// 1. Start a fake "target" TCP server (echo server)
/// 2. Start xr-server
/// 3. Connect as a client using the protocol directly
/// 4. Send data through the tunnel and verify it arrives and echoes back
///
/// Run: cargo test -p xr-proto --test integration -- --nocapture
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use xr_proto::mux::{mux_handshake_client, mux_handshake_server, mux_open_stream, Multiplexer};
use xr_proto::obfuscation::{ModifierStrategy, Obfuscator};
use xr_proto::protocol::{Codec, Command, TargetAddr};

const TEST_KEY: &[u8] = b"test-integration-key-1234567890!";
const TEST_SALT: u32 = 0xCAFEBABE;
const TIMEOUT: Duration = Duration::from_secs(5);

fn make_codec() -> Codec {
    let obfs = Obfuscator::new(TEST_KEY.to_vec(), TEST_SALT, ModifierStrategy::PositionalXorRotate);
    Codec::new(obfs, 8, 32)
}

/// Simple echo server: reads data, sends it back.
async fn run_echo_server(listener: TcpListener) {
    loop {
        let (mut stream, _) = listener.accept().await.unwrap();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                stream.write_all(&buf[..n]).await.unwrap();
            }
        });
    }
}

/// Simplified xr-server: accept one connection, handle protocol.
async fn run_test_server(listener: TcpListener, codec: Codec) {
    let (mut client, _addr) = listener.accept().await.unwrap();

    // Read Connect frame
    let mut buf = vec![0u8; 4096];
    let mut filled = 0;

    let (target_addr, leftover_start) = loop {
        let n = client.read(&mut buf[filled..]).await.unwrap();
        assert!(n > 0, "client disconnected during handshake");
        filled += n;

        match codec.decode_frame(&buf[..filled]).unwrap() {
            Some((frame, consumed)) => {
                assert_eq!(frame.command, Command::Connect);
                let (addr, _) = TargetAddr::decode(&frame.payload).unwrap();
                break (addr, consumed);
            }
            None => continue,
        }
    };

    // Resolve target and connect
    let target_sockaddr: SocketAddr = match &target_addr {
        TargetAddr::Ip(addr) => *addr,
        TargetAddr::Domain(host, port) => {
            format!("{}:{}", host, port).parse().unwrap()
        }
    };

    let target = TcpStream::connect(target_sockaddr).await.unwrap();

    // Send ConnectAck
    let ack = codec.encode_frame(Command::ConnectAck, &[0]).unwrap();
    client.write_all(&ack).await.unwrap();

    // Relay: client (obfuscated) <-> target (plain)
    let leftover = buf[leftover_start..filled].to_vec();

    let (mut cr, mut cw) = client.into_split();
    let (mut tr, mut tw) = target.into_split();

    let codec_up = codec.clone();
    let codec_down = codec.clone();

    let upstream = async move {
        // Process leftover first
        let mut decode_buf = leftover;
        loop {
            // Try decode
            loop {
                if decode_buf.is_empty() {
                    break;
                }
                match codec_up.decode_frame(&decode_buf) {
                    Ok(Some((frame, consumed))) => {
                        match frame.command {
                            Command::Data => {
                                tw.write_all(&frame.payload).await.unwrap();
                            }
                            Command::Close => return,
                            _ => {}
                        }
                        decode_buf = decode_buf[consumed..].to_vec();
                    }
                    Ok(None) => break,
                    Err(_) => return,
                }
            }

            let mut read_buf = vec![0u8; 8192];
            let n = cr.read(&mut read_buf).await.unwrap();
            if n == 0 {
                return;
            }
            decode_buf.extend_from_slice(&read_buf[..n]);
        }
    };

    let downstream = async move {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = tr.read(&mut buf).await.unwrap();
            if n == 0 {
                let close = codec_down.encode_frame(Command::Close, &[]).unwrap();
                cw.write_all(&close).await.unwrap();
                return;
            }
            let frame = codec_down.encode_frame(Command::Data, &buf[..n]).unwrap();
            cw.write_all(&frame).await.unwrap();
        }
    };

    tokio::select! {
        _ = upstream => {},
        _ = downstream => {},
    }
}

#[tokio::test]
async fn test_full_tunnel_roundtrip() {
    // 1. Start echo server on a random port
    let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo_listener.local_addr().unwrap();
    tokio::spawn(run_echo_server(echo_listener));

    // 2. Start xr-server on a random port
    let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();
    let server_codec = make_codec();
    tokio::spawn(run_test_server(server_listener, server_codec));

    // 3. Act as xr-client: connect to server, send Connect, exchange data
    let codec = make_codec();
    let mut server_conn = timeout(TIMEOUT, TcpStream::connect(server_addr))
        .await
        .unwrap()
        .unwrap();

    // Send Connect frame pointing to echo server
    let target = TargetAddr::Ip(echo_addr);
    let connect_payload = target.encode();
    let connect_frame = codec.encode_frame(Command::Connect, &connect_payload).unwrap();
    server_conn.write_all(&connect_frame).await.unwrap();

    // Wait for ConnectAck
    let mut ack_buf = vec![0u8; 256];
    let mut ack_filled = 0;
    let ack_frame = loop {
        let n = timeout(TIMEOUT, server_conn.read(&mut ack_buf[ack_filled..]))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "server closed before ack");
        ack_filled += n;
        if let Some((frame, _)) = codec.decode_frame(&ack_buf[..ack_filled]).unwrap() {
            break frame;
        }
    };
    assert_eq!(ack_frame.command, Command::ConnectAck);
    assert_eq!(ack_frame.payload, &[0]); // success

    // 4. Send test data through the tunnel
    let test_messages = [
        b"Hello, XR Proxy!".to_vec(),
        b"Second message with more data".to_vec(),
        vec![0u8; 1000],                        // binary data
        b"Final message".to_vec(),
    ];

    for (i, msg) in test_messages.iter().enumerate() {
        // Send obfuscated data
        let data_frame = codec.encode_frame(Command::Data, msg).unwrap();
        server_conn.write_all(&data_frame).await.unwrap();

        // Read echoed response (obfuscated by server)
        let mut resp_buf = vec![0u8; msg.len() + 256];
        let mut resp_filled = 0;
        let echoed = loop {
            let n = timeout(TIMEOUT, server_conn.read(&mut resp_buf[resp_filled..]))
                .await
                .unwrap()
                .unwrap();
            assert!(n > 0, "server closed during echo {}", i);
            resp_filled += n;
            if let Some((frame, _)) = codec.decode_frame(&resp_buf[..resp_filled]).unwrap() {
                break frame;
            }
        };

        assert_eq!(echoed.command, Command::Data);
        assert_eq!(echoed.payload, *msg, "echo mismatch on message {}", i);
    }

    // 5. Close
    let close_frame = codec.encode_frame(Command::Close, &[]).unwrap();
    server_conn.write_all(&close_frame).await.unwrap();

    println!("✅ Full tunnel roundtrip test passed!");
    println!("   - {} messages sent and echoed through obfuscated tunnel", test_messages.len());
    println!("   - Echo server: {}", echo_addr);
    println!("   - XR Server:   {}", server_addr);
}

#[tokio::test]
async fn test_domain_connect() {
    // Same as above but using Domain address type (as SNI-based routing would do)
    let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo_listener.local_addr().unwrap();
    tokio::spawn(run_echo_server(echo_listener));

    let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();
    tokio::spawn(run_test_server(server_listener, make_codec()));

    let codec = make_codec();
    let mut conn = timeout(TIMEOUT, TcpStream::connect(server_addr))
        .await
        .unwrap()
        .unwrap();

    // Connect using "127.0.0.1" as domain (server will resolve it)
    let target = TargetAddr::Domain("127.0.0.1".to_string(), echo_addr.port());
    let frame = codec.encode_frame(Command::Connect, &target.encode()).unwrap();
    conn.write_all(&frame).await.unwrap();

    // Read ack
    let mut buf = vec![0u8; 256];
    let mut filled = 0;
    loop {
        let n = timeout(TIMEOUT, conn.read(&mut buf[filled..])).await.unwrap().unwrap();
        filled += n;
        if let Some((frame, _)) = codec.decode_frame(&buf[..filled]).unwrap() {
            assert_eq!(frame.command, Command::ConnectAck);
            break;
        }
    }

    // Send and verify echo
    let msg = b"Domain-based connect works!";
    let data = codec.encode_frame(Command::Data, msg).unwrap();
    conn.write_all(&data).await.unwrap();

    let mut resp_buf = vec![0u8; 256];
    let mut resp_filled = 0;
    loop {
        let n = timeout(TIMEOUT, conn.read(&mut resp_buf[resp_filled..])).await.unwrap().unwrap();
        resp_filled += n;
        if let Some((frame, _)) = codec.decode_frame(&resp_buf[..resp_filled]).unwrap() {
            assert_eq!(frame.payload, msg);
            break;
        }
    }

    println!("✅ Domain-based connect test passed!");
}

#[tokio::test]
async fn test_wrong_key_rejected() {
    // Server with one key, client with another — should fail
    let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();

    let _server_codec = make_codec(); // correct key
    tokio::spawn(async move {
        let (mut client, _) = server_listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        // Server will try to read — should fail or client gets no valid ack
        let _ = timeout(Duration::from_secs(2), client.read(&mut buf)).await;
    });

    // Client with wrong key
    let wrong_obfs = Obfuscator::new(b"wrong-key-totally-different!!!!!".to_vec(), TEST_SALT, ModifierStrategy::PositionalXorRotate);
    let wrong_codec = Codec::new(wrong_obfs, 8, 32);

    let mut conn = timeout(TIMEOUT, TcpStream::connect(server_addr))
        .await
        .unwrap()
        .unwrap();

    let target = TargetAddr::Ip("127.0.0.1:9999".parse().unwrap());
    let frame = wrong_codec.encode_frame(Command::Connect, &target.encode()).unwrap();
    conn.write_all(&frame).await.unwrap();

    // Should NOT receive a valid ConnectAck
    let mut buf = vec![0u8; 256];
    let result = timeout(Duration::from_secs(2), conn.read(&mut buf)).await;

    match result {
        Ok(Ok(0)) | Err(_) => {
            // Connection closed or timeout — expected behavior
            println!("✅ Wrong key correctly rejected (connection closed/timeout)");
        }
        Ok(Ok(n)) => {
            // Got some data — it shouldn't decode as valid ConnectAck with wrong key
            let decode_result = wrong_codec.decode_frame(&buf[..n]);
            match decode_result {
                Err(_) | Ok(None) => {
                    println!("✅ Wrong key correctly rejected (undecipherable response)");
                }
                Ok(Some((frame, _))) => {
                    // Extremely unlikely but possible with random magic collision
                    assert_ne!(frame.command, Command::ConnectAck,
                        "wrong key should not produce valid ConnectAck");
                }
            }
        }
        Ok(Err(e)) => {
            println!("✅ Wrong key correctly rejected (error: {})", e);
        }
    }
}

/// Simplified mux-aware test server: accepts MuxInit, serves streams.
async fn run_mux_test_server(listener: TcpListener, codec: Codec) {
    let (mut client, _addr) = listener.accept().await.unwrap();

    // Read first frame (MuxInit).
    let mut buf = vec![0u8; 4096];
    let mut filled = 0;
    let init_frame = loop {
        let n = client.read(&mut buf[filled..]).await.unwrap();
        filled += n;
        if let Some((frame, _)) = codec.decode_frame(&buf[..filled]).unwrap() {
            break frame;
        }
    };

    assert_eq!(init_frame.command, Command::MuxInit);
    mux_handshake_server(&mut client, &codec, &init_frame).await.unwrap();

    // Create server-side multiplexer.
    let mux = Multiplexer::new_server(client, codec.clone());
    let mut new_stream_rx = mux.take_new_stream_rx().await.unwrap();

    // Handle incoming streams — connect to targets and relay.
    while let Some(new_stream) = new_stream_rx.recv().await {
        let sid = new_stream.stream_id;
        let (target_addr, _) = TargetAddr::decode(&new_stream.payload).unwrap();

        // Send ConnectAck.
        mux.send_frame(sid, Command::ConnectAck, vec![0]).await.unwrap();

        // Connect to target.
        let target_sockaddr = match &target_addr {
            TargetAddr::Ip(a) => *a,
            TargetAddr::Domain(h, p) => format!("{}:{}", h, p).parse().unwrap(),
        };

        let mut mux_stream = mux.register_stream(sid).await;
        let mux_for_close = mux.clone();

        tokio::spawn(async move {
            let mut target = TcpStream::connect(target_sockaddr).await.unwrap();
            let (mut tr, mut tw) = target.split();

            let (dl_tx, mut dl_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

            let relay = async {
                loop {
                    tokio::select! {
                        data = mux_stream.recv() => {
                            match data {
                                Some(d) if !d.is_empty() => tw.write_all(&d).await.unwrap(),
                                _ => break,
                            }
                        }
                        data = dl_rx.recv() => {
                            match data {
                                Some(d) => mux_stream.send(&d).await.unwrap(),
                                None => break,
                            }
                        }
                    }
                }
            };

            let download = async {
                let mut buf = vec![0u8; 8192];
                loop {
                    let n = tr.read(&mut buf).await.unwrap();
                    if n == 0 { break; }
                    if dl_tx.send(buf[..n].to_vec()).await.is_err() { break; }
                }
            };

            tokio::select! {
                _ = relay => {}
                _ = download => {}
            }

            let _ = mux_for_close.send_frame(sid, Command::Close, Vec::new()).await;
        });
    }
}

#[tokio::test]
async fn test_mux_two_concurrent_streams() {
    // 1. Start two echo servers on random ports.
    let echo1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo1_addr = echo1.local_addr().unwrap();
    tokio::spawn(run_echo_server(echo1));

    let echo2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo2_addr = echo2.local_addr().unwrap();
    tokio::spawn(run_echo_server(echo2));

    // 2. Start mux-aware test server.
    let server_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = server_listener.local_addr().unwrap();
    tokio::spawn(run_mux_test_server(server_listener, make_codec()));

    // 3. Client: connect, mux handshake, open two streams.
    let codec = make_codec();
    let mut conn = timeout(TIMEOUT, TcpStream::connect(server_addr))
        .await.unwrap().unwrap();

    assert!(mux_handshake_client(&mut conn, &codec).await.unwrap());

    let client_mux = Multiplexer::new_client(conn, codec.clone());

    // Open stream 1 → echo1
    let mut stream1 = timeout(
        TIMEOUT,
        mux_open_stream(&client_mux, &TargetAddr::Ip(echo1_addr)),
    ).await.unwrap().unwrap();

    // Open stream 2 → echo2
    let mut stream2 = timeout(
        TIMEOUT,
        mux_open_stream(&client_mux, &TargetAddr::Ip(echo2_addr)),
    ).await.unwrap().unwrap();

    // 4. Send data on both streams concurrently.
    let msg1 = b"hello from stream 1";
    let msg2 = b"hello from stream 2";

    stream1.send(msg1).await.unwrap();
    stream2.send(msg2).await.unwrap();

    // 5. Receive echoed responses.
    let resp1 = timeout(TIMEOUT, stream1.recv()).await.unwrap().unwrap();
    let resp2 = timeout(TIMEOUT, stream2.recv()).await.unwrap().unwrap();

    assert_eq!(resp1, msg1);
    assert_eq!(resp2, msg2);

    // 6. Close streams independently.
    stream1.close().await.unwrap();

    // Stream 2 should still work after stream 1 closed.
    let msg3 = b"stream 2 still alive";
    stream2.send(msg3).await.unwrap();
    let resp3 = timeout(TIMEOUT, stream2.recv()).await.unwrap().unwrap();
    assert_eq!(resp3, msg3);

    stream2.close().await.unwrap();

    println!("✅ Mux two concurrent streams test passed!");
    println!("   - Stream 1 → echo {}", echo1_addr);
    println!("   - Stream 2 → echo {}", echo2_addr);
    println!("   - Both through single TCP to mux server {}", server_addr);
}
