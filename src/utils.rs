use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

use anyhow::{bail, Context};
use tokio::net::UdpSocket;

pub fn join_multicast(
    multicast_address: &SocketAddr,
    interface_addr: &SocketAddr,
    interface_index: u32,
) -> anyhow::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};

    let mc_addr = multicast_address.ip();
    // it's an error to not use a proper mDNS address
    if !mc_addr.is_multicast() {
        bail!("Needs a proper multicast address");
    }

    let socket = match mc_addr {
        IpAddr::V4(ref mc_v4addr) => {
            let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
                .context("Failed to create IPV4 socket")?;

            match interface_addr.ip() {
                IpAddr::V4(ref addr4) => {
                    socket
                        .join_multicast_v4(mc_v4addr, addr4)
                        .expect("join_multicast_v4");
                    socket.set_multicast_loop_v4(true)?;
                    socket
                }
                IpAddr::V6(_) => {
                    bail!("Interface Mismatch")
                }
            }
        }
        IpAddr::V6(ref mdns_v6) => {
            let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))
                .context("Failed to create IPV6 socket")?;

            socket.set_only_v6(true)?;
            socket
                .join_multicast_v6(mdns_v6, interface_index)
                .context("Failed to join multicast address")?;
            socket
        }
    };

    socket.set_nonblocking(true).context("Nonblocking Error")?;
    socket.set_reuse_address(true).context("Reuse Addr Error")?;
    #[cfg(unix)] // this is currently restricted to Unix's in socket2
    socket.set_reuse_port(true).context("Reuse Port Error")?;

    socket.bind(&socket2::SockAddr::from(*multicast_address))?;

    let socket: std::net::UdpSocket = socket.into();

    Ok(UdpSocket::from_std(socket).context("Failed to create Tokio UDP Socket")?)
}

pub fn get_multicast_ssdp_socket_v4() -> anyhow::Result<UdpSocket> {
    let ssdp_address = SocketAddr::from_str("239.255.255.250:1900")?;
    let interface_address = SocketAddr::from_str("0.0.0.0:0")?;
    let interface_index = 0;

    join_multicast(&ssdp_address, &interface_address, interface_index)
}

use lru_cache::LruCache;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use pnet::datalink;
use regex::Regex;
use std::cmp::Ordering;

use crate::types::{Container, Item, ListItem, ListItemWrapper, Res};

const FRAGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'[')
    .add(b']');

pub fn natural_order_strings(first_string: String, second_string: String) -> Ordering {
    let regex = Regex::new("[^0-9]+|[0-9]+").unwrap();
    let tokens1: Vec<String> = regex
        .captures_iter(&first_string)
        .map(|c| c.get(0).unwrap().as_str().to_lowercase().to_owned())
        .collect();
    println!("{:?}", tokens1);

    let tokens2: Vec<String> = regex
        .captures_iter(&second_string)
        .map(|c| c.get(0).unwrap().as_str().to_lowercase().to_owned())
        .collect();
    println!("{:?}", tokens2);

    let (longer, shorter, is_first_longer) = if tokens1.len() > tokens2.len() {
        (tokens1, tokens2, true)
    } else {
        (tokens1, tokens2, true)
    };

    for (a, b) in longer.iter().zip(shorter.iter()) {
        let cmp = if let (Ok(x), Ok(y)) = (a.parse::<u128>(), b.parse::<u128>()) {
            x.cmp(&y)
        } else {
            a.cmp(b)
        };
        if cmp != Ordering::Equal {
            if is_first_longer {
                return cmp;
            } else {
                return match cmp {
                    Ordering::Greater => Ordering::Less,
                    Ordering::Less => Ordering::Greater,
                    Ordering::Equal => Ordering::Equal,
                };
            }
        }
    }
    if is_first_longer {
        Ordering::Greater
    } else {
        Ordering::Less
    }
}

pub struct ReadDirectoryReturnType {
    pub list_items: Vec<ListItemWrapper>,
    pub id_counter: u64,
}

pub fn get_local_ips() -> Vec<IpAddr> {
    let interfaces = datalink::interfaces();
    let locations = interfaces
        .iter()
        .filter(|&x| x.is_multicast() && x.is_broadcast())
        .map(|location| location.ips.iter().map(|x| x.ip()).filter(|x| x.is_ipv4()))
        .flatten()
        .collect();

    locations
    // location.ips.iter().map(|x| x.ip()).collect()
}

pub async fn read_directory(
    hostname: String,
    path: String,
    parent_id: u64,
    mut id_counter: u64,
) -> ReadDirectoryReturnType {
    let mut entries = tokio::fs::read_dir(&path).await.unwrap();

    let mut list_items = vec![];

    while let Ok(entry) = entries.next_entry().await {
        if let Some(entry) = entry {
            if let Ok(entry_type) = entry.file_type().await {
                if entry_type.is_dir() {
                    list_items.push(ListItemWrapper {
                        list_item: ListItem::Container(Container {
                            id: id_counter,
                            parent_id: parent_id,
                            title: entry.file_name().into_string().unwrap(),
                            class: "object.container.storageFolder".to_string(),
                        }),
                        id: id_counter,
                        dir: Some(entry.path().to_str().unwrap().to_string()),
                    })
                } else {
                    let file_name = entry.file_name().into_string().unwrap();
                    let file_path = entry.path().to_str().unwrap().to_string();
                    let file_path = utf8_percent_encode(&file_path, FRAGMENT).to_string();

                    if file_name.ends_with(".mp4") || file_name.ends_with(".mkv") {
                        list_items.push(ListItemWrapper {
                            list_item: ListItem::Item(Item {
                                id: id_counter,
                                parent_id: parent_id,
                                title: entry.file_name().into_string().unwrap(),
                                class: "object.item.videoItem".to_string(),
                                res: Res {
                                    protocol_info: "http-get:*:video/x-matroska:*".to_string(),
                                    content: format!(
                                        "http://{}/agni-files/?filename={}",
                                        hostname, file_path
                                    ),
                                },
                            }),
                            id: id_counter,
                            dir: None,
                        })
                    }
                }
            }
        } else {
            break;
        }
        id_counter += 1;
    }

    list_items.sort_by(|a, b| {
        let compute_string =
            |list_item_wrapper: &ListItemWrapper| match list_item_wrapper.list_item.clone() {
                ListItem::Container(x) => format!("D{}", x.title),
                ListItem::Item(x) => format!("F{}", x.title),
            };
        natural_order_strings(compute_string(a), compute_string(b))
    });
    ReadDirectoryReturnType {
        list_items,
        id_counter,
    }
}

pub fn get_cache() -> LruCache<u64, Vec<ListItemWrapper>> {
    let mut initial_list_items = vec![ListItemWrapper {
        list_item: ListItem::Container(Container {
            id: 1,
            parent_id: 0,
            title: "Documents".to_string(),
            class: "object.container.storageFolder".to_string(),
        }),
        id: 1,
        dir: Some("/mnt/Documents".into()),
    }];

    for (i, x) in initial_list_items.iter_mut().enumerate() {
        let id = i as u64 + 1;
        x.id = id;
        match &mut x.list_item {
            ListItem::Container(x) => x.id = id,
            ListItem::Item(x) => x.id = id,
        }
    }

    let mut cache: LruCache<u64, Vec<ListItemWrapper>> = LruCache::new(100);

    cache.insert(0, initial_list_items);

    cache
}
