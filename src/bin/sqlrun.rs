// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::{sync::Arc, time::Duration};

use arrow_array::RecordBatch;
use arrow_cast::pretty::pretty_format_batches;
use arrow_flight::{
    sql::client::FlightSqlServiceClient, utils::flight_data_to_batches, FlightData,
};
use arrow_schema::{ArrowError, Schema};
use clap::Parser;
use futures::TryStreamExt;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tracing_log::log::info;

use datafusion_benchmarks::tpch::get_query_sql;

/// A ':' separated key value pair
#[derive(Debug, Clone)]
struct KeyValue<K, V> {
    pub key: K,
    pub value: V,
}

impl<K, V> std::str::FromStr for KeyValue<K, V>
where
    K: std::str::FromStr,
    V: std::str::FromStr,
    K::Err: std::fmt::Display,
    V::Err: std::fmt::Display,
{
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let parts = s.splitn(2, ':').collect::<Vec<_>>();
        match parts.as_slice() {
            [key, value] => {
                let key = K::from_str(key).map_err(|e| e.to_string())?;
                let value = V::from_str(value).map_err(|e| e.to_string())?;
                Ok(Self { key, value })
            }
            _ => Err(format!(
                "Invalid key value pair - expected 'KEY:VALUE' got '{s}'"
            )),
        }
    }
}

#[derive(Debug, Clone, Parser)]
struct ClientArgs {
    /// Additional headers.
    ///
    /// Values should be key value pairs separated by ':'
    #[arg(long, value_delimiter = ',')]
    headers: Vec<KeyValue<String, String>>,

    /// Username
    #[arg(value_parser, long = "username", default_value = "admin")]
    username: Option<String>,

    /// Password
    #[arg(value_parser, long = "password", default_value = "password")]
    password: Option<String>,

    /// Auth token.
    #[arg(long)]
    token: Option<String>,

    /// Use TLS.
    #[arg(long)]
    tls: bool,

    /// Server host.
    #[arg(value_parser, long = "host", default_value = "0.0.0.0")]
    host: String,

    /// Server port.
    #[arg(long)]
    #[arg(value_parser, long = "port", default_value = "50060")]
    port: Option<u16>,
}

#[derive(Debug, Parser)]
struct Args {
    /// Client args.
    #[clap(flatten)]
    client_args: ClientArgs,

    /// SQL query.
    #[arg(value_parser, long = "query", default_value = "select * from region;")]
    query: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    setup_logging();

    // queries that work 1, 13, 14, 16, 22
    // queries that go through but return no results 3, 4, 6, 10, 11, 12, 17, 18, 19, 20
    // queries that break 2, 5, 7, 8, 9, 15, 21

    let working_array = [1, 3, 4, 6, 10, 11, 12, 13, 14, 16, 17, 18, 19, 20, 22];

    for id in working_array.iter() {
        println!("\nQuery Id = {}", id);
        let sql = &get_query_sql(*id);
        for query in sql {
            do_query(args.client_args.clone(), &query[0]).await
        }
    }

    //do_query(args.client_args, &args.query).await
}

async fn do_query(client_args: ClientArgs, query_str: &str) {
    let mut client = setup_client(client_args).await.expect("setup client");

    let query = query_str.into();
    let info = client.execute(query).await.expect("prepare statement");
    info!("got flight info");

    let schema = Arc::new(Schema::try_from(info.clone()).expect("valid schema"));
    let mut batches = Vec::with_capacity(info.endpoint.len() + 1);
    batches.push(RecordBatch::new_empty(schema));
    info!("decoded schema");

    for endpoint in info.endpoint {
        let Some(ticket) = &endpoint.ticket else {
            panic!("did not get ticket");
        };
        let flight_data = client.do_get(ticket.clone()).await.expect("do get");
        let flight_data: Vec<FlightData> = flight_data
            .try_collect()
            .await
            .expect("collect data stream");
        let mut endpoint_batches =
            flight_data_to_batches(&flight_data).expect("convert flight data to record batches");
        batches.append(&mut endpoint_batches);
    }
    info!("received data");

    let res = pretty_format_batches(batches.as_slice()).expect("format results");
    println!("{res}");
}

fn setup_logging() {
    tracing_log::LogTracer::init().expect("tracing log init");
    tracing_subscriber::fmt::init();
}

async fn setup_client(args: ClientArgs) -> Result<FlightSqlServiceClient<Channel>, ArrowError> {
    let port = args.port.unwrap_or(if args.tls { 443 } else { 80 });

    let protocol = if args.tls { "https" } else { "http" };

    let mut endpoint = Endpoint::new(format!("{}://{}:{}", protocol, args.host, port))
        .map_err(|_| ArrowError::IoError("Cannot create endpoint".to_string()))?
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(20))
        .tcp_nodelay(true) // Disable Nagle's Algorithm since we don't want packets to wait
        .tcp_keepalive(Option::Some(Duration::from_secs(3600)))
        .http2_keep_alive_interval(Duration::from_secs(300))
        .keep_alive_timeout(Duration::from_secs(20))
        .keep_alive_while_idle(true);

    if args.tls {
        let tls_config = ClientTlsConfig::new();
        endpoint = endpoint
            .tls_config(tls_config)
            .map_err(|_| ArrowError::IoError("Cannot create TLS endpoint".to_string()))?;
    }

    let channel = endpoint
        .connect()
        .await
        .map_err(|e| ArrowError::IoError(format!("Cannot connect to endpoint: {e}")))?;

    let mut client = FlightSqlServiceClient::new(channel);
    info!("connected");

    for kv in args.headers {
        client.set_header(kv.key, kv.value);
    }

    if let Some(token) = args.token {
        client.set_token(token);
        info!("token set");
    }

    match (args.username, args.password) {
        (None, None) => {}
        (Some(username), Some(password)) => {
            client
                .handshake(&username, &password)
                .await
                .expect("handshake");
            info!("performed handshake");
        }
        (Some(_), None) => {
            panic!("when username is set, you also need to set a password")
        }
        (None, Some(_)) => {
            panic!("when password is set, you also need to set a username")
        }
    }

    Ok(client)
}
