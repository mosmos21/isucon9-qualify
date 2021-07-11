use actix_multipart::Multipart;
use actix_web::{middleware, web, App, Error as AWError, HttpResponse, HttpServer};
use bytes::BytesMut;
use futures::TryStreamExt;
use listenfd::ListenFd;
use mysql::prelude::*;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs::File;
use std::sync::Arc;
use std::time;

type Pool = r2d2::Pool<r2d2_mysql::MysqlConnectionManager>;
type BlockingDBError = actix_web::error::BlockingError<mysql::Error>;

const DEFAULT_PAYMENT_SERVICE_URL: &str = "http://localhost:5555";
const DEFAULT_SHIPMENT_SERVICE_URL: &str = "http://localhost:7000";
const ITEM_MIN_PRICE: i32 = 100;
const ITEM_MAX_PRICE: i32 = 1000000;
const ITEM_PRICE_ERR_MSG: &str = "商品価格は100ｲｽｺｲﾝ以上、1,000,000ｲｽｺｲﾝ以下にしてください";
const PAYMENT_SERVICE_ISUCARI_API_KEY: &str = "a15400e46c83635eb181-946abb51ff26a868317c";
const PAYMENT_SERVICE_ISUCARI_SHOP_ID: &str = "11";
const BUMP_CHARGE_SECONDS: i32 = 3 * 60;
const ITEM_PER_PAGE: i32 = 48;
const TRANSACTIONS_PER_PAGE: i32 = 10;
const BCRYPT_COST: i32 = 10;

enum ItemStatus {
    OnSale,
    Trading,
    SoldOut,
    Stop,
    Cancel,
}

enum TransactionEvidenceStatus {
    WaitShipping,
    WaitDone,
    Done,
}

enum ShippingStatus {
    Initial,
    WaitPickup,
    Shipping,
    Done,
}

#[derive(Debug)]
struct MySQLConnectionEnv {
    host: String,
    port: u16,
    user: String,
    db_name: String,
    password: String,
}

impl Default for MySQLConnectionEnv {
    fn default() -> Self {
        let port = if let Ok(port) = env::var("MYSQL_PORT") {
            port.parse().unwrap_or(3306)
        } else {
            3306
        };
        Self {
            host: env::var("MYSQL_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned()),
            port,
            user: env::var("MYSQL_USER").unwrap_or_else(|_| "isucari".to_owned()),
            db_name: env::var("MYSQL_DBNAME").unwrap_or_else(|_| "isucari".to_owned()),
            password: env::var("MYSQL_PASS").unwrap_or_else(|_| "isucari".to_owned()),
        }
    }
}

#[deribe(Debug, Deserialize, Serialize)]
struct Config {
    name: String,
    val: String,
}

#[deribe(Debug, Deserialize, Serialize)]
struct User {
    id: i64,
    account_name: String,
    hashed_password: Vec<u8>,
    address: String,
    num_sell_items: i32,
    last_bump: time::SystemTime,
    created_at: time::SystemTime,
}

struct UserSimple {
    id: i64,
    account_name: String,
    num_sell_items: i32,
}

struct Item {
    id: i64,
    seller_id: i64,
    buyer_id: i64,
    status: ItemStatus,
    name: String,
    price: i32,
    description: String,
    image_name: String,
    category_id: i64,
    created_at: time::SystemTime,
    updated_at: time::SystemTime,
}

struct ItemSimple {
    id: i64,
    seller_id: i64,
    seller: UserSimple,
    status: ItemStatus,
    name: String,
    price: i32,
    image_url: String,
    category_id: i32,
    category: Category,
    created_at: time::SystemTime,
}

struct ItemDetail {
    id: i64,
    seller_id: i64,
    seller: UserSimple,
    buyer_di: i64,
    buyer: UserSimple,
    status: ItemStatus,
    name: String,
    price: i32,
    description: String,
    image_url: String,
    category_id: i32,
    category: Category,
    transaction_evidence_id: i64,
    transaction_evidence_status: TransactionEvidenceStatus,
    shipping_status: ShippingStatus,
    created_at: time::SystemTime,
}

struct TransactionEvidence {
    id: i64,
    seller_id: i64,
    buyer_id: i64,
    status: TransactionEvidenceStatus,
    item_id: i64,
    item_name: String,
    item_price: i32,
    item_description: String,
    item_category_id: i64,
    item_root_category_id: i64,
    create_at: time::SystemTime,
    updated_at: time::SystemTime,
}

struct Shipping {
    transaction_evidence_id: i64,
    status: ShippingStatus,
    item_name: String,
    item_id: i64,
    reserve_id: i64,
    reserve_time: i64,
    to_address: String,
    to_name: String,
    from_address: String,
    from_name: String,
    img_binary: Vec<u8>,
    created_at: time::SystemTime,
    updated_at: time::SystemTime
}

struct Category {
    id: i32,
    parent_id: i32,
    category_name: String,
    parent_category_id: String,
}

struct ReqInitialize {
    payment_service_url: String,
    shipment_service_url: string,
}

struct ResInitialize {
    campaign: i32,
    language: String,
}

struct ResNewItems {
    root_category_id: i32,
    root_category_name: String,
    has_next: bool,
    items: Vec<ItemSimple>,
}

struct ResUserItems {
    user: UserSimple,
    has_next: bool,
    items: Vec<ItemSimple>,
}

struct ResTransactions {
    has_next: bool,
    items: Vec<ItemDetail>,
}

struct ReqRegister {
    account_name: String,
    address: String,
    password: String,
}

struct ReqLogin {
    account_name: String,
    password: String,
}

struct ReqItemEdit {
    csrf_token: String,
    item_id: i64,
    item_price: i32,
}

struct ResItemEdit {
    item_id: i64,
    item_price: i32,
    item_created_at: time::SystemTime,
    item_updated_at: time::SystemTime,
}

struct ReqBuy {
    csrf_token: String,
    item_id: i64,
    token: String,
}

struct ResBuy {
    transaction_evidence_id: i64,
}

struct ResSell {
    id: i64,
}

struct ReqPostShip {
    csrf_token: String,
    item_id: i64,
}

struct ResPostShip {
    path: String,
    reserve_id: string,
}

struct ReqPostShipDone {
    csrf_token: String,
    item_id: i64,
}

struct ReqPostComplete {
    csrf_token: String,
    item_id: i64,
}

struct ReqBump {
    csrf_token: String,
    item_id: i64,
}

struct ResSetting {
    csrf_token: String,
    payment_service_url: String,
    user: User,
    categories: Vec<Category>,
}

#[actix_rt::main]
async fn main() -> std::io::Result<()> {
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG", "actix_server=info,actix_web=info,isuumo=info");
    }
    env_logger::init();

    let mysql_connection_env = Arc::new(MySQLConnectionEnv::default());

    let manager = r2d2_mysql::MysqlConnectionManager::new(
        mysql::OptsBuilder::new()
            .ip_or_hostname(Some(&mysql_connection_env.host))
            .tcp_port(mysql_connection_env.port)
            .user(Some(&mysql_connection_env.user))
            .db_name(Some(&mysql_connection_env.db_name))
            .pass(Some(&mysql_connection_env.password)),
    );
    let pool = r2d2::Pool::builder()
        .max_size(10)
        .build(manager)
        .expect("Failed to create connection pool");

    let mut listenfd = ListenFd::from_env();
    let server = HttpServer::new(move || {
        App::new()
            .data(pool.clone())
            .data(mysql_connection_env.clone())
            .wrap(middleware::Logger::default())
    });
    let server = if let Some(l) = listenfd.take_tcp_listener(0)? {
        server.listen(l)?
    } else {
        server.bind((
            "0.0.0.0",
            std::env::var("SERVER_PORT")
                .map(|port_str| port_str.parse().expect("Failed to parse SERVER_PORT"))
                .unwrap_or(8000),
        ))?
    };
    server.run().await
}