use crate::channel::ChannelConfig;
use crate::connection::{ClientId, Connection};
use crate::endpoint::{Endpoint, EndpointConfig, NetworkInfo};
use crate::error::RenetError;
use crate::protocol::{AuthenticationProtocol, ServerAuthenticationProtocol};
use log::{debug, error, info};
use std::collections::HashMap;
use std::io;
use std::marker::PhantomData;
use std::net::{SocketAddr, UdpSocket};
use std::time::Instant;

use super::handle_connection::HandleConnection;
use super::ServerConfig;

#[derive(Debug, Clone)]
pub enum Event {
    ClientConnected(ClientId),
    ClientDisconnected(ClientId),
}

// TODO: add internal buffer?
pub struct Server<P> {
    config: ServerConfig,
    socket: UdpSocket,
    clients: HashMap<ClientId, Connection>,
    connecting: HashMap<ClientId, HandleConnection>,
    channels_config: HashMap<u8, ChannelConfig>,
    current_time: Instant,
    events: Vec<Event>,
    endpoint_config: EndpointConfig,
    _authentication_protocol: PhantomData<P>,
}

impl<P> Server<P>
where
    P: AuthenticationProtocol + ServerAuthenticationProtocol,
{
    pub fn new(
        socket: UdpSocket,
        config: ServerConfig,
        endpoint_config: EndpointConfig,
        channels_config: HashMap<u8, ChannelConfig>,
    ) -> Result<Self, RenetError> {
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            clients: HashMap::new(),
            connecting: HashMap::new(),
            config,
            channels_config,
            endpoint_config,
            current_time: Instant::now(),
            events: Vec::new(),
            _authentication_protocol: PhantomData,
        })
    }

    pub fn has_clients(&self) -> bool {
        !self.clients.is_empty()
    }

    pub fn get_event(&mut self) -> Option<Event> {
        self.events.pop()
    }

    fn find_client_by_addr(&mut self, addr: &SocketAddr) -> Option<&mut Connection> {
        self.clients
            .values_mut()
            .find(|c| c.addr == *addr)
    }

    fn find_connection_by_addr(&mut self, addr: &SocketAddr) -> Option<&mut HandleConnection> {
        self.connecting.values_mut().find(|c| c.addr == *addr)
    }

    pub fn get_client_network_info(&mut self, client_id: ClientId) -> Option<&NetworkInfo> {
        if let Some(connection) = self.clients.get_mut(&client_id) {
            connection.endpoint.update_sent_bandwidth();
            connection.endpoint.update_received_bandwidth();
            return Some(connection.endpoint.network_info());
        }
        None
    }

    pub fn send_message_to_all_clients(&mut self, channel_id: u8, message: Box<[u8]>) {
        for connection in self.clients.values_mut() {
            connection.send_message(channel_id, message.clone());
        }
    }

    pub fn send_message_to_client(
        &mut self,
        client_id: ClientId,
        channel_id: u8,
        message: Box<[u8]>,
    ) {
        if let Some(connection) = self.clients.get_mut(&client_id) {
            connection.send_message(channel_id, message);
        }
    }

    pub fn get_messages_from_client(
        &mut self,
        client_id: ClientId,
        channel_id: u8,
    ) -> Option<Vec<Box<[u8]>>> {
        if let Some(client) = self.clients.get_mut(&client_id) {
            return Some(client.receive_all_messages_from_channel(channel_id));
        }
        None
    }

    pub fn get_messages_from_channel(&mut self, channel_id: u8) -> Vec<(ClientId, Vec<Box<[u8]>>)> {
        let mut clients = Vec::new();
        for (client_id, connection) in self.clients.iter_mut() {
            let messages = connection.receive_all_messages_from_channel(channel_id);
            if !messages.is_empty() {
                clients.push((*client_id, messages));
            }
        }
        clients
    }

    pub fn get_clients_id(&self) -> Vec<ClientId> {
        self.clients.keys().map(|x| x.clone()).collect()
    }

    pub fn update(&mut self, current_time: Instant) {
        if let Err(e) = self.process_events(current_time) {
            error!("Error while processing events:\n{:?}", e);
        }
        self.update_pending_connections();
    }

    pub fn send_packets(&mut self) {
        for (client_id, connection) in self.clients.iter_mut() {
            match connection.get_packet() {
                Ok(Some(payload)) => {
                    if let Err(e) = connection.send_payload(&payload, &self.socket) {
                        error!("Failed to send payload for client {}: {:?}", client_id, e);
                    }
                }
                Ok(None) => {}
                Err(_) => error!("Failed to get packet for client {}.", client_id),
            }
        }
    }

    pub fn process_payload_from(
        &mut self,
        payload: Box<[u8]>,
        addr: &SocketAddr,
    ) -> Result<(), RenetError> {
        if let Some(client) = self.find_client_by_addr(addr) {
            client.process_payload(payload);
            return Ok(());
        }

        if self.clients.len() >= self.config.max_clients {
            // TODO: send denied connection
            debug!("Connection Denied to addr {}, server is full.", addr);
            return Ok(());
        }

        match self.find_connection_by_addr(addr) {
            Some(connection) => {
                if let Err(e) = connection.process_payload(payload) {
                    error!("{}", e)
                }
            }
            None => {
                let protocol = P::from_payload(payload)?;
                let id = protocol.id();
                info!("Created new protocol from payload with client id {}", id);
                let new_connection = HandleConnection::new(protocol.id(), addr.clone(), protocol);
                self.connecting.insert(id, new_connection);
            }
        };

        Ok(())
    }

    fn process_events(&mut self, current_time: Instant) -> Result<(), RenetError> {
        for connection in self.clients.values_mut() {
            connection.update_channels_current_time(current_time);
        }
        let mut buffer = vec![0u8; self.config.max_payload_size];
        loop {
            match self.socket.recv_from(&mut buffer) {
                Ok((len, addr)) => {
                    let payload = buffer[..len].to_vec().into_boxed_slice();
                    if let Err(e) = self.process_payload_from(payload, &addr) {
                        error!("Error while processing events:\n{:?}", e);
                    }
                }
                // Break from the loop if would block
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(());
                }
                Err(e) => return Err(RenetError::IOError(e)),
            };
        }
    }

    fn update_pending_connections(&mut self) {
        let mut connected_clients = vec![];
        for connection in self.connecting.values_mut() {
            if connection.protocol.is_authenticated() {
                connected_clients.push(connection.client_id);
            } else {
                if let Ok(Some(payload)) = connection.protocol.create_payload() {
                    if let Err(e) = self.socket.send_to(&payload, connection.addr) {
                        error!("Failed to send protocol packet {}", e);
                    }
                }
            }
        }
        for client_id in connected_clients {
            let handle_connection = self
                .connecting
                .remove(&client_id)
                .expect("Should only connect existing clients.");
            if self.clients.len() >= self.config.max_clients {
                debug!(
                    "Connection from {} successfuly stablished but server was full.",
                    handle_connection.addr
                );
                // TODO: deny connection, max player
                continue;
            }

            debug!(
                "Connection stablished with client {} ({}).",
                handle_connection.client_id, handle_connection.addr,
            );

            let endpoint: Endpoint = Endpoint::new(self.endpoint_config.clone());
            let security_service = handle_connection.protocol.build_security_interface();
            let mut connection = Connection::new(
                handle_connection.addr,
                endpoint,
                security_service,
            );

            for (channel_id, channel_config) in self.channels_config.iter() {
                let channel = channel_config.new_channel(self.current_time);
                connection.add_channel(*channel_id, channel);
            }

            self.events.push(Event::ClientConnected(handle_connection.client_id));
            self.clients.insert(handle_connection.client_id, connection);
        }
    }
}

/*
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{SocketAddr, UdpSocket};

    #[test]
    fn server_client_connecting_flow() {
        let socket = UdpSocket::bind("127.0.0.1:8080").unwrap();
        let server_config = ServerConfig::default();
        let mut server = Server::new(socket, server_config).unwrap();

        let client_addr: SocketAddr = "127.0.0.1:8081".parse().unwrap();
        let packet = ConnectionPacket::ConnectionRequest(0);
        server.process_packet_from(packet, &client_addr).unwrap();
        assert_eq!(server.connecting.len(), 1);

        let packet = ConnectionPacket::ChallengeResponse;
        server.process_packet_from(packet, &client_addr).unwrap();
        server.update_pending_connections();
        assert_eq!(server.connecting.len(), 1);
        assert_eq!(server.clients.len(), 0);

        let packet = ConnectionPacket::HeartBeat;
        server.process_packet_from(packet, &client_addr).unwrap();
        server.update_pending_connections();
        assert_eq!(server.connecting.len(), 0);
        assert_eq!(server.clients.len(), 1);
        assert!(server.clients.contains_key(&0));
    }
}
*/