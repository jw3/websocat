extern crate hyper;
extern crate websocket;

use self::hyper::http::h1::Incoming;
use self::hyper::method::Method;
use self::hyper::uri::RequestUri;

use self::websocket::WebSocketError;
use futures::future::{Future,err};
use futures::stream::Stream;

use std::cell::RefCell;
use std::rc::Rc;

use self::websocket::server::upgrade::async::IntoWs;

use super::readdebt::{DebtHandling, ReadDebt};
use super::ws_peer::{Mode1, PeerForWs, WsReadWrapper, WsWriteWrapper};
use super::{box_up_err, io_other_error, BoxedNewPeerFuture, Peer};
use super::{ConstructParams, PeerConstructor, Specifier};

#[derive(Debug)]
pub struct WsServer<T: Specifier>(pub T);
impl<T: Specifier> Specifier for WsServer<T> {
    fn construct(&self, cp: ConstructParams) -> PeerConstructor {
        let mode1 = if cp.program_options.websocket_text_mode {
            Mode1::Text
        } else {
            Mode1::Binary
        };
        let restrict_uri = Rc::new(cp.program_options.restrict_uri.clone());
        let inner = self.0.construct(cp.clone());
        inner.map(move |p| {
            ws_upgrade_peer(
                p,
                mode1,
                cp.program_options.read_debt_handling,
                restrict_uri.clone(),
            )
        })
    }
    specifier_boilerplate!(typ=WebSocket noglobalstate has_subspec);
    self_0_is_subspecifier!(proxy_is_multiconnect);
}
specifier_class!(
    name = WsServerClass,
    target = WsServer,
    prefixes = ["ws-upgrade:", "upgrade-ws:", "ws-u:", "u-ws:"],
    arg_handling = subspec,
    overlay = true,
    MessageOriented,
    MulticonnectnessDependsOnInnerType,
    help = r#"
WebSocket upgrader / raw server. Specify your own protocol instead of usual TCP. [A]

All other WebSocket server modes actually use this overlay under the hood.

Example: serve incoming connection from socat

    socat tcp-l:1234,fork,reuseaddr exec:'websocat -t ws-u\:stdio\: mirror\:'
"#
);

specifier_alias!(
    name = WsTcpServerClass,
    prefixes = ["ws-listen:", "ws-l:", "l-ws:", "listen-ws:"],
    alias = "ws-u:tcp-l:",
    help = r#"
WebSocket server. Argument is host and port to listen.

Example: Dump all incoming websocket data to console

    websocat ws-l:127.0.0.1:8808 -

Example: the same, but more verbose:

    websocat ws-l:tcp-l:127.0.0.1:8808 reuse:-
"#
);

specifier_alias!(
    name = WsInetdServerClass,
    prefixes = ["inetd-ws:", "ws-inetd:"],
    alias = "ws-u:inetd:",
    help = r#"
WebSocket inetd server. [A]

TODO: transfer the example here
"#
);

specifier_alias!(
    name = WsUnixServerClass,
    prefixes = ["l-ws-unix:"],
    alias = "ws-u:unix-l:",
    help = r#"
WebSocket UNIX socket-based server. [A]
"#
);

specifier_alias!(
    name = WsAbstractUnixServerClass,
    prefixes = ["l-ws-abstract:"],
    alias = "ws-l:abstract-l:",
    help = r#"
WebSocket abstract-namespaced UNIX socket server. [A]
"#
);

/* 

     if x == "" {
                Err("Specify underlying protocol for ws-l:")?;
            }
            if let Some(c) = x.chars().next() {
                if c.is_numeric() || c == '[' {
                    // Assuming user uses old format like ws-l:127.0.0.1:8080
                    return spec(&("ws-l:tcp-l:".to_owned() + x));
                }
            }
            boxup(super::ws_server_peer::WsUpgrade(spec(x)?))
*/

fn http_serve(p:Peer, _incoming:Option<Incoming<(Method, RequestUri)>>) -> Box<Future<Item=(), Error=()>> {
    let content = b"HTTP/1.1 400 Bad Reqeust\r\nServer: websocat\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nOnly WebSocket connections are welcome here\n";
    let reply = super::trivial_peer::get_literal_peer_now(content.to_vec());
    let co = super::my_copy::CopyOptions {
        buffer_size: 4096,
        once: false,
        stop_on_reader_zero_read: true,
    };
    Box::new(super::my_copy::copy(reply, p.1, co).map(|_|()).map_err(drop))
}

pub fn ws_upgrade_peer(
    inner_peer: Peer,
    mode1: Mode1,
    ws_read_debt_handling: DebtHandling,
    restrict_uri: Rc<Option<String>>,
) -> BoxedNewPeerFuture {
    let step1 = PeerForWs(inner_peer);
    let step2: Box<
        Future<Item = self::websocket::server::upgrade::async::Upgrade<_>, Error = _>,
    > = step1.into_ws();
    let step3 = step2
        .or_else(|(innerpeer, hyper_incoming, _bytesmut, e)| {
            http_serve(innerpeer.0, hyper_incoming)
            .then(|_|
                err(WebSocketError::IoError(io_other_error(e)))
            )
        })
        .and_then(
            move |x| -> Box<Future<Item = Peer, Error = websocket::WebSocketError>> {
                info!("Incoming connection to websocket: {}", x.request.subject.1);
                debug!("{:?}", x.request);
                debug!("{:?}", x.headers);
                if let Some(ref restrict_uri) = *restrict_uri {
                    let check_passed = match x.request.subject.1 {
                        hyper::uri::RequestUri::AbsolutePath(ref x) if x == restrict_uri => true,
                        _ => false,
                    };
                    if !check_passed {
                        return Box::new(
                            x.reject()
                                .and_then(|_| {
                                    warn!("Incoming request URI doesn't match the --restrict-uri value");
                                    ::futures::future::err(::util::simple_err(
                                        "Request URI doesn't match --restrict-uri parameter"
                                            .to_string(),
                                    ))
                                })
                                .map_err(|e| websocket::WebSocketError::IoError(io_other_error(e))),
                        )
                            as Box<Future<Item = Peer, Error = websocket::WebSocketError>>;
                    }
                };
                Box::new(x.accept().map(move |(y, headers)| {
                    debug!("{:?}", headers);
                    info!("Upgraded");
                    let (sink, stream) = y.split();
                    let mpsink = Rc::new(RefCell::new(sink));

                    let ws_str = WsReadWrapper {
                        s: stream,
                        pingreply: mpsink.clone(),
                        debt: ReadDebt(Default::default(), ws_read_debt_handling),
                    };
                    let ws_sin =
                        WsWriteWrapper(mpsink, mode1, true /* send Close on shutdown */);

                    Peer::new(ws_str, ws_sin)
                })) as Box<Future<Item = Peer, Error = websocket::WebSocketError>>
            },
        );
    let step4 = step3.map_err(box_up_err);
    Box::new(step4) as BoxedNewPeerFuture
}
