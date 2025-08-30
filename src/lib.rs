#![warn(missing_docs)]
//! # Actix Request-Reply Cache
//!
//! A Redis-backed caching middleware for Actix Web that enables response caching.
//!
//! This library implements efficient HTTP response caching using Redis as a backend store,
//! with functionality for fine-grained cache control through predicates that can examine
//! request context to determine cacheability.
//!
//! ## Features
//!
//! - Redis-backed HTTP response caching
//! - Configurable TTL (time-to-live) for cached responses
//! - Customizable cache key prefix
//! - Maximum cacheable response size configuration
//! - Flexible cache control through predicate functions
//! - Respects standard HTTP cache control headers
//!
//! ## Example
//!
//! ```rust
//! use actix_web::{web, App, HttpServer};
//! use actix_request_reply_cache::RedisCacheMiddlewareBuilder;
//!
//! #[actix_web::main]
//! async fn main() -> std::io::Result<()> {
//!     // Create the cache middleware
//!     let cache = RedisCacheMiddlewareBuilder::new("redis://127.0.0.1:6379")
//!         .ttl(60)  // Cache for 60 seconds
//!         .cache_if(|ctx| {
//!             // Only cache GET requests without Authorization header
//!             ctx.method == "GET" && !ctx.headers.contains_key("Authorization")
//!         })
//!         .build()
//!         .await;
//!         
//!     HttpServer::new(move || {
//!         App::new()
//!             .wrap(cache.clone())
//!             .service(web::resource("/").to(|| async { "Hello world!" }))
//!     })
//!     .bind(("127.0.0.1", 8080))?
//!     .run()
//!     .await
//! }
//! ```
use actix_web::{
    body::{BodySize, BoxBody, EitherBody, MessageBody},
    dev::{forward_ready, Payload, Service, ServiceRequest, ServiceResponse, Transform},
    http::header::HeaderMap,
    web::{Bytes, BytesMut},
    Error, HttpMessage,
};
use futures::{
    future::{ready, LocalBoxFuture, Ready},
    StreamExt,
};
use pin_project_lite::pin_project;
use redis::{aio::MultiplexedConnection, AsyncCommands};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{future::Future, marker::PhantomData, pin::Pin, rc::Rc};
use std::{
    sync::Arc,
    task::{Context, Poll},
};

/// Context containing request information for cache operations.
///
/// This struct contains information about the current request that can be used for:
/// - Making caching decisions through predicate functions
/// - Generating custom cache keys
pub struct CacheContext<'a> {
    /// The HTTP method of the request (e.g., "GET", "POST")
    pub method: &'a str,
    /// The request path
    pub path: &'a str,
    /// The query string from the request URL
    pub query_string: &'a str,
    /// HTTP headers from the request
    pub headers: &'a HeaderMap,
    /// The request body as a byte slice
    pub body: &'a serde_json::Value,
}

/// Function type for cache decision predicates.
///
/// This type represents functions that take a `CacheDecisionContext` and return
/// a boolean indicating whether the response should be cached.
type CachePredicate = Arc<dyn Fn(&CacheContext) -> bool + Send + Sync>;

/// Function type for custom cache key generation.
///
/// This type represents functions that take a `CacheDecisionContext` and return
/// a string to be used as the base for the cache key.
type CacheKeyFn = Arc<dyn Fn(&CacheContext) -> String + Send + Sync>;

/// Redis-backed caching middleware for Actix Web.
///
/// This middleware intercepts responses, caches them in Redis, and serves
/// cached responses for subsequent matching requests when available.
#[derive(Clone)]
pub struct RedisCacheMiddleware {
    redis_conn: Option<MultiplexedConnection>,
    redis_url: String,
    ttl: u64,
    max_cacheable_size: usize,
    cache_prefix: String,
    cache_if: CachePredicate,
    cache_key_fn: Option<CacheKeyFn>,
}

/// Builder for configuring and creating the `RedisCacheMiddleware`.
///
/// Provides a fluent interface for configuring cache parameters such as TTL,
/// maximum cacheable size, cache key prefix, and cache decision predicates.
pub struct RedisCacheMiddlewareBuilder {
    redis_url: String,
    ttl: u64,
    max_cacheable_size: usize,
    cache_prefix: String,
    cache_if: CachePredicate,
    cache_key_fn: Option<CacheKeyFn>,
}

impl RedisCacheMiddlewareBuilder {
    /// Creates a new builder with the given Redis URL.
    ///
    /// # Arguments
    ///
    /// * `redis_url` - The Redis connection URL (e.g., "redis://127.0.0.1:6379")
    ///
    /// # Returns
    ///
    /// A new `RedisCacheMiddlewareBuilder` with default settings:
    /// - TTL: 3600 seconds (1 hour)
    /// - Max cacheable size: 1MB
    /// - Cache prefix: "cache:"
    /// - Cache predicate: cache all responses
    pub fn new(redis_url: impl Into<String>) -> Self {
        Self {
            redis_url: redis_url.into(),
            ttl: 3600,                       // 1 hour default
            max_cacheable_size: 1024 * 1024, // 1MB default
            cache_prefix: "cache:".to_string(),
            cache_if: Arc::new(|_| true), // Default: cache everything
            cache_key_fn: None,           // Default: use standard key generation
        }
    }

    /// Sets the TTL (time-to-live) for cached responses in seconds.
    ///
    /// # Arguments
    ///
    /// * `seconds` - The number of seconds a response should remain in the cache
    ///
    /// # Returns
    ///
    /// Self for method chaining
    pub fn ttl(mut self, seconds: u64) -> Self {
        self.ttl = seconds;
        self
    }

    /// Sets the maximum size of responses that can be cached, in bytes.
    ///
    /// Responses larger than this size will not be cached.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The maximum cacheable response size in bytes
    ///
    /// # Returns
    ///
    /// Self for method chaining
    pub fn max_cacheable_size(mut self, bytes: usize) -> Self {
        self.max_cacheable_size = bytes;
        self
    }

    /// Sets the prefix used for Redis cache keys.
    ///
    /// # Arguments
    ///
    /// * `prefix` - The string prefix to use for all cache keys
    ///
    /// # Returns
    ///
    /// Self for method chaining
    pub fn cache_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.cache_prefix = prefix.into();
        self
    }

    /// Set a predicate function to determine if a response should be cached
    ///
    /// Example:
    /// ```
    /// builder.cache_if(|ctx| {
    ///     // Only cache GET requests
    ///     if ctx.method != "GET" {
    ///         return false;
    ///     }
    ///     
    ///     // Don't cache if Authorization header is present
    ///     if ctx.headers.contains_key("Authorization") {
    ///         return false;
    ///     }
    ///     
    ///     // Don't cache responses to paths that start with /admin
    ///     if ctx.path.starts_with("/admin") {
    ///         return false;
    ///     }
    ///
    ///     // Don't cache for a specific route if its body contains some field
    ///     if ctx.path.starts_with("/api/users") && ctx.method == "POST" {
    ///         return ctx.body.get("role").and_then(|r| r.as_str()) != Some("admin");
    ///     }
    ///     true
    /// })
    /// ```
    pub fn cache_if<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&CacheContext) -> bool + Send + Sync + 'static,
    {
        self.cache_if = Arc::new(predicate);
        self
    }

    /// Set a custom function to determine the cache key.
    ///
    /// By default, cache keys are based on HTTP method, path, query string, and
    /// (if present) a hash of the request body. This method lets you specify a custom
    /// function to generate the base key before hashing.
    ///
    /// Example:
    /// ```
    /// builder.with_cache_key(|ctx| {
    ///     // Only use method and path for the cache key (ignore query params and body)
    ///     format!("{}:{}", ctx.method, ctx.path)
    ///     
    ///     // Or include specific query parameters
    ///     // let user_id = ctx.query_string.split('&')
    ///     //     .find(|p| p.starts_with("user_id="))
    ///     //     .unwrap_or("");
    ///     // format!("{}:{}:{}", ctx.method, ctx.path, user_id)
    /// })
    /// ```
    pub fn with_cache_key<F>(mut self, key_fn: F) -> Self
    where
        F: Fn(&CacheContext) -> String + Send + Sync + 'static,
    {
        self.cache_key_fn = Some(Arc::new(key_fn));
        self
    }

    /// Builds and returns the configured `RedisCacheMiddleware`.
    ///
    /// # Returns
    ///
    /// A new `RedisCacheMiddleware` instance configured with the settings from this builder.
    pub fn build(self) -> RedisCacheMiddleware {
        RedisCacheMiddleware {
            redis_conn: None,
            redis_url: self.redis_url,
            ttl: self.ttl,
            max_cacheable_size: self.max_cacheable_size,
            cache_prefix: self.cache_prefix,
            cache_if: self.cache_if,
            cache_key_fn: self.cache_key_fn,
        }
    }
}

impl RedisCacheMiddleware {
    /// Creates a new `RedisCacheMiddleware` with default settings.
    ///
    /// This is a convenience method that uses the builder with default settings.
    ///
    /// # Arguments
    ///
    /// * `redis_url` - The Redis connection URL
    ///
    /// # Returns
    ///
    /// A new `RedisCacheMiddleware` instance with default settings.
    pub fn new(redis_url: &str) -> Self {
        RedisCacheMiddlewareBuilder::new(redis_url).build()
    }
}

/// Service implementation for the Redis cache middleware.
///
/// This struct is created by the `RedisCacheMiddleware` and handles
/// the actual interception of requests and responses for caching.
pub struct RedisCacheMiddlewareService<S> {
    service: Rc<S>,
    redis_conn: Option<MultiplexedConnection>,
    redis_url: String,
    ttl: u64,
    max_cacheable_size: usize,
    cache_prefix: String,
    cache_if: CachePredicate,
    cache_key_fn: Option<CacheKeyFn>,
}

#[derive(Serialize, Deserialize)]
struct CachedResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl<S, B> Transform<S, ServiceRequest> for RedisCacheMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: 'static + MessageBody,
{
    type Response = ServiceResponse<EitherBody<B, BoxBody>>;
    type Error = Error;
    type Transform = RedisCacheMiddlewareService<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    /// Creates a new transform of the input service.
    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(RedisCacheMiddlewareService {
            service: Rc::new(service),
            redis_conn: self.redis_conn.clone(),
            redis_url: self.redis_url.clone(),
            ttl: self.ttl,
            max_cacheable_size: self.max_cacheable_size,
            cache_prefix: self.cache_prefix.clone(),
            cache_if: self.cache_if.clone(),
            cache_key_fn: self.cache_key_fn.clone(),
        }))
    }
}

// Define the wrapper structure for your response future
pin_project! {
    struct CacheResponseFuture<S, B>
    where
        B: MessageBody,
        S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    {
        #[pin]
        fut: S::Future,
        should_cache: bool,
        cache_key: String,
        redis_conn: Option<MultiplexedConnection>,
        redis_url: String,
        ttl: u64,
        max_cacheable_size: usize,
        _marker: PhantomData<B>,
    }
}

// Implement the Future trait for your response future
impl<S, B> Future for CacheResponseFuture<S, B>
where
    B: MessageBody + 'static,
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
{
    type Output = Result<ServiceResponse<EitherBody<B, BoxBody>>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let res = futures_util::ready!(this.fut.poll(cx))?;

        let status = res.status();
        let headers = res.headers().clone();
        let should_cache = *this.should_cache && status.is_success();

        if !should_cache {
            return Poll::Ready(Ok(res.map_body(|_, b| EitherBody::left(b))));
        }

        let cache_key = this.cache_key.clone();
        let redis_url = this.redis_url.clone();
        let redis_conn = this.redis_conn.clone();
        let ttl = *this.ttl;
        let max_size = *this.max_cacheable_size;

        let res = res.map_body(move |_, body| {
            let filtered_headers = headers
                .iter()
                .filter(|(name, _)| {
                    !["connection", "transfer-encoding", "content-length"]
                        .contains(&name.as_str().to_lowercase().as_str())
                })
                .map(|(name, value)| {
                    (
                        name.to_string(),
                        value.to_str().unwrap_or_default().to_string(),
                    )
                })
                .collect::<Vec<_>>();

            EitherBody::right(BoxBody::new(CacheableBody {
                body: body.boxed(),
                status: status.as_u16(),
                headers: filtered_headers,
                body_accum: BytesMut::new(),
                cache_key,
                redis_conn,
                redis_url,
                ttl,
                max_size,
            }))
        });

        Poll::Ready(Ok(res))
    }
}

// Define the body wrapper that will accumulate data
pin_project! {
    struct CacheableBody {
        #[pin]
        body: BoxBody,
        status: u16,
        headers: Vec<(String, String)>,
        body_accum: BytesMut,
        cache_key: String,
        redis_conn: Option<MultiplexedConnection>,
        redis_url: String,
        ttl: u64,
        max_size: usize,
    }

    impl PinnedDrop for CacheableBody {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();

            let body_bytes = this.body_accum.clone().freeze();
            let status = *this.status;
            let headers = this.headers.clone();
            let cache_key = this.cache_key.clone();
            let mut redis_conn = this.redis_conn.take();
            let redis_url = this.redis_url.clone();
            let ttl = *this.ttl;
            let max_size = *this.max_size;

            if !body_bytes.is_empty() && body_bytes.len() <= max_size {
                actix_web::rt::spawn(async move {
                    let cached_response = CachedResponse {
                        status,
                        headers,
                        body: body_bytes.to_vec(),
                    };

                    if let Ok(serialized) = rmp_serde::to_vec(&cached_response) {
                        if redis_conn.is_none() {
                            let client = redis::Client::open(redis_url.as_str())
                                .expect("Failed to connect to Redis");

                            let conn = client
                                .get_multiplexed_async_connection()
                                .await
                                .expect("Failed to get Redis connection");

                            redis_conn = Some(conn);
                        }

                        if let Some(conn) = redis_conn.as_mut() {
                            let _: Result<(), redis::RedisError> =
                                conn.set_ex(cache_key, serialized, ttl).await;
                        }
                    }
                });
            }
        }
    }
}

impl MessageBody for CacheableBody {
    type Error = <BoxBody as MessageBody>::Error;

    fn size(&self) -> BodySize {
        self.body.size()
    }

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Self::Error>>> {
        let this = self.project();

        // Poll the inner body and accumulate data
        match this.body.poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.body_accum.extend_from_slice(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S, B> Service<ServiceRequest> for RedisCacheMiddlewareService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<EitherBody<B, BoxBody>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, mut req: ServiceRequest) -> Self::Future {
        // Skip caching if Cache-Control says no-cache/no-store
        if let Some(cache_control) = req.headers().get("Cache-Control") {
            if let Ok(cache_control_str) = cache_control.to_str() {
                if cache_control_str.contains("no-cache") || cache_control_str.contains("no-store")
                {
                    let fut = self.service.call(req);
                    return Box::pin(async move {
                        let res = fut.await?;
                        Ok(res.map_body(|_, b| EitherBody::left(b)))
                    });
                }
            }
        }

        let redis_url = self.redis_url.clone();
        let mut redis_conn = self.redis_conn.clone();
        let expiration = self.ttl;
        let max_cacheable_size = self.max_cacheable_size;
        let cache_prefix = self.cache_prefix.clone();
        let service = Rc::clone(&self.service);
        let cache_if = self.cache_if.clone();
        let cache_key_fn = self.cache_key_fn.clone();

        Box::pin(async move {
            let body_bytes = req
                .take_payload()
                .fold(BytesMut::new(), move |mut body, chunk| async {
                    if let Ok(chunk) = chunk {
                        body.extend_from_slice(&chunk);
                    }
                    body
                })
                .await;

            let cache_ctx = CacheContext {
                method: req.method().as_str(),
                path: req.path(),
                query_string: req.query_string(),
                headers: req.headers(),
                body: &serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null),
            };

            let should_cache = cache_if(&cache_ctx);

            // Generate cache key using custom function if provided, otherwise use the default
            let base_key = if let Some(key_fn) = &cache_key_fn {
                key_fn(&cache_ctx)
            } else if body_bytes.is_empty() {
                format!(
                    "{}:{}:{}",
                    req.method().as_str(),
                    req.path(),
                    req.query_string()
                )
            } else {
                let body_hash = hex::encode(Sha256::digest(&body_bytes));
                format!(
                    "{}:{}:{}:{}",
                    req.method().as_str(),
                    req.path(),
                    req.query_string(),
                    body_hash
                )
            };

            req.set_payload(Payload::from(Bytes::from(body_bytes.clone())));

            let hashed_key = hex::encode(Sha256::digest(base_key.as_bytes()));
            let cache_key = format!("{}{}", cache_prefix, hashed_key);

            let cached_result: Option<Vec<u8>> = if should_cache {
                if redis_conn.is_none() {
                    let client = redis::Client::open(redis_url.as_str())
                        .expect("Failed to connect to Redis");

                    let conn = client
                        .get_multiplexed_async_connection()
                        .await
                        .expect("Failed to get Redis connection");

                    redis_conn = Some(conn);
                }

                let conn = redis_conn.as_mut().unwrap();
                conn.get(&cache_key).await.unwrap_or(None)
            } else {
                None
            };

            if let Some(cached_data) = cached_result {
                log::debug!("Cache hit for {}", cache_key);

                match rmp_serde::from_slice::<CachedResponse>(&cached_data) {
                    Ok(cached_response) => {
                        let mut response = actix_web::HttpResponse::build(
                            actix_web::http::StatusCode::from_u16(cached_response.status)
                                .unwrap_or(actix_web::http::StatusCode::OK),
                        );

                        for (name, value) in cached_response.headers {
                            response.insert_header((name, value));
                        }

                        response.insert_header(("X-Cache", "HIT"));

                        let resp = response.body(cached_response.body);
                        return Ok(req
                            .into_response(resp)
                            .map_body(|_, b| EitherBody::right(BoxBody::new(b))));
                    }
                    Err(e) => {
                        log::error!("Failed to deserialize cached response: {}", e);
                    }
                }
            }

            log::debug!("Cache miss for {}", cache_key);
            let future = CacheResponseFuture::<S, B> {
                fut: service.call(req),
                should_cache,
                cache_key,
                redis_conn,
                redis_url,
                ttl: expiration,
                max_cacheable_size,
                _marker: PhantomData,
            };

            future.await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{http::header, test::TestRequest};

    #[actix_web::test]
    async fn test_builder_default_values() {
        let builder = RedisCacheMiddlewareBuilder::new("redis://localhost");
        assert_eq!(builder.ttl, 3600);
        assert_eq!(builder.max_cacheable_size, 1024 * 1024);
        assert_eq!(builder.cache_prefix, "cache:");
        assert_eq!(builder.redis_url, "redis://localhost");
    }

    #[actix_web::test]
    async fn test_builder_custom_values() {
        let builder = RedisCacheMiddlewareBuilder::new("redis://localhost")
            .ttl(60)
            .max_cacheable_size(512 * 1024)
            .cache_prefix("custom:");

        assert_eq!(builder.ttl, 60);
        assert_eq!(builder.max_cacheable_size, 512 * 1024);
        assert_eq!(builder.cache_prefix, "custom:");
    }

    #[actix_web::test]
    async fn test_builder_custom_predicate() {
        let builder = RedisCacheMiddlewareBuilder::new("redis://localhost")
            .cache_if(|ctx| ctx.method == "GET");

        // Test the predicate
        let get_ctx = CacheContext {
            method: "GET",
            path: "/test",
            query_string: "",
            headers: &header::HeaderMap::new(),
            body: &serde_json::Value::Null,
        };

        let post_ctx = CacheContext {
            method: "POST",
            path: "/test",
            query_string: "",
            headers: &header::HeaderMap::new(),
            body: &serde_json::Value::Null,
        };

        // The predicate should now only allow GET requests
        assert!((builder.cache_if)(&get_ctx));
        assert!(!(builder.cache_if)(&post_ctx));
    }

    #[actix_web::test]
    async fn test_cache_key_generation() {
        // Create a simple request
        let req = TestRequest::get().uri("/test").to_srv_request();

        // Extract the relevant parts for key generation
        let method = req.method().as_str();
        let path = req.path();
        let query_string = req.query_string();

        // Generate key manually as done in the middleware
        let base_key = format!("{}:{}:{}", method, path, query_string);
        let hashed_key = hex::encode(Sha256::digest(base_key.as_bytes()));
        let cache_key = format!("test:{}", hashed_key);

        // Now verify this matches what our middleware would generate
        let expected_key = format!(
            "test:{}",
            hex::encode(Sha256::digest("GET:/test:".to_string().as_bytes()))
        );

        assert_eq!(cache_key, expected_key);
    }

    #[actix_web::test]
    async fn test_cache_key_with_body() {
        // Test case for when request has a body
        let body_bytes = b"test body";
        let body_hash = hex::encode(Sha256::digest(body_bytes));

        // Generate key manually as done in the middleware
        let base_key = format!("{}:{}:{}:{}", "POST", "/test", "", body_hash);
        let hashed_key = hex::encode(Sha256::digest(base_key.as_bytes()));
        let cache_key = format!("test:{}", hashed_key);

        // Expected key when body is present
        let expected_key = format!(
            "test:{}",
            hex::encode(Sha256::digest(
                format!("POST:/test::{}", body_hash).as_bytes()
            ))
        );

        assert_eq!(cache_key, expected_key);
    }

    #[actix_web::test]
    async fn test_cacheable_methods() {
        // Test different HTTP methods with default predicate
        let builder = RedisCacheMiddlewareBuilder::new("redis://localhost");
        let default_predicate = builder.cache_if;

        let methods = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];

        for method in methods {
            let ctx = CacheContext {
                method,
                path: "/test",
                query_string: "",
                headers: &header::HeaderMap::new(),
                body: &serde_json::Value::Null,
            };

            // Default predicate should cache all methods
            assert!(
                (default_predicate)(&ctx),
                "Method {} should be cacheable by default",
                method
            );
        }

        // Test with a custom predicate that only caches GET and HEAD
        let custom_builder = RedisCacheMiddlewareBuilder::new("redis://localhost")
            .cache_if(|ctx| matches!(ctx.method, "GET" | "HEAD"));

        for method in methods {
            let ctx = CacheContext {
                method,
                path: "/test",
                query_string: "",
                headers: &header::HeaderMap::new(),
                body: &serde_json::Value::Null,
            };

            // Check if method should be cached according to our custom predicate
            let should_cache = matches!(method, "GET" | "HEAD");
            assert_eq!(
                (custom_builder.cache_if)(&ctx),
                should_cache,
                "Method {} should be cacheable: {}",
                method,
                should_cache
            );
        }
    }

    #[actix_web::test]
    async fn test_predicate_with_headers() {
        // Test predicate behavior with different headers

        // Create a predicate that doesn't cache requests with Authorization header
        let predicate = |ctx: &CacheContext| !ctx.headers.contains_key("Authorization");

        // Test with empty headers
        let mut headers = header::HeaderMap::new();
        let ctx_no_auth = CacheContext {
            method: "GET",
            path: "/test",
            query_string: "",
            headers: &headers,
            body: &serde_json::Value::Null,
        };

        assert!(
            predicate(&ctx_no_auth),
            "Request without Authorization should be cached"
        );

        // Test with Authorization header
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_static("Bearer token"),
        );

        let ctx_with_auth = CacheContext {
            method: "GET",
            path: "/test",
            query_string: "",
            headers: &headers,
            body: &serde_json::Value::Null,
        };

        assert!(
            !predicate(&ctx_with_auth),
            "Request with Authorization should not be cached"
        );
    }

    #[actix_web::test]
    async fn test_predicate_with_path_patterns() {
        // Test predicate behavior with different path patterns

        // Create a predicate that doesn't cache admin paths
        let predicate =
            |ctx: &CacheContext| !ctx.path.starts_with("/admin") && !ctx.path.contains("/private/");

        // Test paths that should be cached
        let cacheable_paths = ["/", "/api/users", "/public/resource", "/api/v1/data"];

        for path in cacheable_paths {
            let ctx = CacheContext {
                method: "GET",
                path,
                query_string: "",
                headers: &header::HeaderMap::new(),
                body: &serde_json::Value::Null,
            };

            assert!(predicate(&ctx), "Path {} should be cacheable", path);
        }

        // Test paths that should not be cached
        let non_cacheable_paths = ["/admin", "/admin/users", "/users/private/profile"];

        for path in non_cacheable_paths {
            let ctx = CacheContext {
                method: "GET",
                path,
                query_string: "",
                headers: &header::HeaderMap::new(),
                body: &serde_json::Value::Null,
            };

            assert!(!predicate(&ctx), "Path {} should not be cacheable", path);
        }
    }

    #[actix_web::test]
    async fn test_cached_response_serialization() {
        // Test that CachedResponse can be properly serialized and deserialized
        let cached_response = CachedResponse {
            status: 200,
            headers: vec![
                ("Content-Type".to_string(), "text/plain".to_string()),
                ("X-Test".to_string(), "value".to_string()),
            ],
            body: b"test response".to_vec(),
        };

        // Serialize
        let serialized = rmp_serde::to_vec(&cached_response).unwrap();

        // Deserialize
        let deserialized: CachedResponse = rmp_serde::from_slice(&serialized).unwrap();

        // Verify fields match
        assert_eq!(deserialized.status, 200);
        assert_eq!(deserialized.headers.len(), 2);
        assert_eq!(deserialized.headers[0].0, "Content-Type");
        assert_eq!(deserialized.headers[0].1, "text/plain");
        assert_eq!(deserialized.headers[1].0, "X-Test");
        assert_eq!(deserialized.headers[1].1, "value");
        assert_eq!(deserialized.body, b"test response");
    }

    #[actix_web::test]
    async fn test_custom_cache_key() {
        // Create a builder with a custom cache key function that only uses method and path
        let builder = RedisCacheMiddlewareBuilder::new("redis://localhost")
            .with_cache_key(|ctx| format!("{}:{}", ctx.method, ctx.path));

        // Create a function that extracts our cache key generation logic
        let get_key = |method: &str, path: &str, query: &str, body: &[u8]| {
            // Create a CacheDecisionContext
            let headers = header::HeaderMap::new();
            let body_json = serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
            let ctx = CacheContext {
                method,
                path,
                query_string: query,
                headers: &headers,
                body: &body_json,
            };

            // Get the base key using our cache key function
            let base_key = if let Some(key_fn) = &builder.cache_key_fn {
                key_fn(&ctx)
            } else {
                format!("{}:{}:{}", method, path, query)
            };

            // Hash it and apply prefix as done in the middleware
            let hashed_key = hex::encode(Sha256::digest(base_key.as_bytes()));
            format!("{}:{}", builder.cache_prefix, hashed_key)
        };

        // Test with different query strings that should now produce the same cache key
        let key1 = get_key("GET", "/users", "", b"");
        let key2 = get_key("GET", "/users", "page=1", b"");
        let key3 = get_key("GET", "/users", "page=2", b"");

        // Keys should be the same since our custom function ignores query string
        assert_eq!(key1, key2);
        assert_eq!(key1, key3);

        // Test with different methods that should produce different cache keys
        let key_get = get_key("GET", "/resource", "", b"");
        let key_post = get_key("POST", "/resource", "", b"");

        // Should be different keys
        assert_ne!(key_get, key_post);
    }
}
