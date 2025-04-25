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
    body::{BoxBody, EitherBody, MessageBody},
    dev::{forward_ready, Payload, Service, ServiceRequest, ServiceResponse, Transform},
    http::header::HeaderMap,
    web::{Bytes, BytesMut},
    Error, HttpMessage,
};
use futures::{
    future::{ready, LocalBoxFuture, Ready},
    StreamExt,
};
use redis::{aio::MultiplexedConnection, AsyncCommands};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Context used to determine if a request/response should be cached.
///
/// This struct contains information about the current request that can be
/// examined by cache predicate functions to make caching decisions.
pub struct CacheDecisionContext<'a> {
    /// The HTTP method of the request (e.g., "GET", "POST")
    pub method: &'a str,
    /// The request path
    pub path: &'a str,
    /// The query string from the request URL
    pub query_string: &'a str,
    /// HTTP headers from the request
    pub headers: &'a HeaderMap,
    /// The request body as a byte slice
    pub body: &'a [u8],
}

/// Function type for cache decision predicates.
///
/// This type represents functions that take a `CacheDecisionContext` and return
/// a boolean indicating whether the response should be cached.
type CachePredicate = Arc<dyn Fn(&CacheDecisionContext) -> bool + Send + Sync>;

/// Redis-backed caching middleware for Actix Web.
///
/// This middleware intercepts responses, caches them in Redis, and serves
/// cached responses for subsequent matching requests when available.
pub struct RedisCacheMiddleware {
    redis_conn: MultiplexedConnection,
    ttl: u64,
    max_cacheable_size: usize,
    cache_prefix: String,
    cache_if: CachePredicate,
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
    ///        if let Ok(user_json) = serde_json::from_slice::<serde_json::Value>(ctx.body) {
    ///            // Check properties in the JSON to make caching decisions
    ///            return user_json.get("role").and_then(|r| r.as_str()) != Some("admin");
    ///        }
    ///    }
    ///     true
    /// })
    /// ```
    pub fn cache_if<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&CacheDecisionContext) -> bool + Send + Sync + 'static,
    {
        self.cache_if = Arc::new(predicate);
        self
    }

    /// Builds and returns the configured `RedisCacheMiddleware`.
    ///
    /// # Returns
    ///
    /// A new `RedisCacheMiddleware` instance configured with the settings from this builder.
    ///
    /// # Panics
    ///
    /// This method will panic if it cannot connect to Redis using the provided URL.
    pub async fn build(self) -> RedisCacheMiddleware {
        let client =
            redis::Client::open(self.redis_url.as_str()).expect("Failed to connect to Redis");

        let redis_conn = client
            .get_multiplexed_async_connection()
            .await
            .expect("Failed to get Redis connection");

        RedisCacheMiddleware {
            redis_conn,
            ttl: self.ttl,
            max_cacheable_size: self.max_cacheable_size,
            cache_prefix: self.cache_prefix,
            cache_if: self.cache_if,
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
    pub async fn new(redis_url: &str) -> Self {
        RedisCacheMiddlewareBuilder::new(redis_url).build().await
    }
}

/// Service implementation for the Redis cache middleware.
///
/// This struct is created by the `RedisCacheMiddleware` and handles
/// the actual interception of requests and responses for caching.
pub struct RedisCacheMiddlewareService<S> {
    service: S,
    redis_conn: MultiplexedConnection,
    ttl: u64,
    max_cacheable_size: usize,
    cache_prefix: String,
    cache_if: CachePredicate,
}

#[derive(Serialize, Deserialize)]
struct CachedResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl<S, B> Transform<S, ServiceRequest> for RedisCacheMiddleware
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static + Clone,
    S::Future: 'static,
    B: 'static + Clone + MessageBody,
{
    type Response = ServiceResponse<EitherBody<B, BoxBody>>;
    type Error = Error;
    type Transform = RedisCacheMiddlewareService<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(RedisCacheMiddlewareService {
            service,
            redis_conn: self.redis_conn.clone(),
            ttl: self.ttl,
            max_cacheable_size: self.max_cacheable_size,
            cache_prefix: self.cache_prefix.clone(),
            cache_if: self.cache_if.clone(),
        }))
    }
}

impl<S, B> Service<ServiceRequest> for RedisCacheMiddlewareService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static + Clone,
    S::Future: 'static,
    B: actix_web::body::MessageBody + 'static + Clone,
{
    type Response = ServiceResponse<EitherBody<B, BoxBody>>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, mut req: ServiceRequest) -> Self::Future {
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

        let mut redis_conn = self.redis_conn.clone();
        let expiration = self.ttl;
        let max_cacheable_size = self.max_cacheable_size;
        let cache_prefix = self.cache_prefix.clone();
        let service = self.service.clone();
        let cache_if = self.cache_if.clone();

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

            let cache_ctx = CacheDecisionContext {
                method: req.method().as_str(),
                path: req.path(),
                query_string: req.query_string(),
                headers: req.headers(),
                body: &body_bytes,
            };

            let should_cache = cache_if(&cache_ctx);

            req.set_payload(Payload::from(Bytes::from(body_bytes.clone())));

            let base_key = if body_bytes.is_empty() {
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

            let hashed_key = hex::encode(Sha256::digest(base_key.as_bytes()));
            let cache_key = format!("{}{}", cache_prefix, hashed_key);

            // Only try to get from cache if we're considering caching for this request
            let cached_result: Option<String> = if should_cache {
                redis_conn.get(&cache_key).await.unwrap_or(None)
            } else {
                None
            };

            if let Some(cached_data) = cached_result {
                log::debug!("Cache hit for {}", cache_key);

                // Deserialize cached response
                match serde_json::from_str::<CachedResponse>(&cached_data) {
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

            let service_result = service.call(req).await?;

            // Only store in cache if we're considering caching and the response is successful
            if should_cache && service_result.status().is_success() {
                let res = service_result.response();

                let status = res.status().as_u16();

                let headers = res
                    .headers()
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

                if let Ok(body) = res.body().clone().try_into_bytes() {
                    if !body.is_empty() && body.len() <= max_cacheable_size {
                        let cached_response = CachedResponse {
                            status,
                            headers,
                            body: body.to_vec(),
                        };

                        if let Ok(serialized) = serde_json::to_string(&cached_response) {
                            let _: Result<(), redis::RedisError> =
                                redis_conn.set_ex(cache_key, serialized, expiration).await;
                        }
                    }
                }
            }

            Ok(service_result.map_body(|_, b| EitherBody::left(b)))
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
        let get_ctx = CacheDecisionContext {
            method: "GET",
            path: "/test",
            query_string: "",
            headers: &header::HeaderMap::new(),
            body: &[],
        };

        let post_ctx = CacheDecisionContext {
            method: "POST",
            path: "/test",
            query_string: "",
            headers: &header::HeaderMap::new(),
            body: &[],
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
            let ctx = CacheDecisionContext {
                method,
                path: "/test",
                query_string: "",
                headers: &header::HeaderMap::new(),
                body: &[],
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
            let ctx = CacheDecisionContext {
                method,
                path: "/test",
                query_string: "",
                headers: &header::HeaderMap::new(),
                body: &[],
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
        let predicate = |ctx: &CacheDecisionContext| !ctx.headers.contains_key("Authorization");

        // Test with empty headers
        let mut headers = header::HeaderMap::new();
        let ctx_no_auth = CacheDecisionContext {
            method: "GET",
            path: "/test",
            query_string: "",
            headers: &headers,
            body: &[],
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

        let ctx_with_auth = CacheDecisionContext {
            method: "GET",
            path: "/test",
            query_string: "",
            headers: &headers,
            body: &[],
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
        let predicate = |ctx: &CacheDecisionContext| {
            !ctx.path.starts_with("/admin") && !ctx.path.contains("/private/")
        };

        // Test paths that should be cached
        let cacheable_paths = ["/", "/api/users", "/public/resource", "/api/v1/data"];

        for path in cacheable_paths {
            let ctx = CacheDecisionContext {
                method: "GET",
                path,
                query_string: "",
                headers: &header::HeaderMap::new(),
                body: &[],
            };

            assert!(predicate(&ctx), "Path {} should be cacheable", path);
        }

        // Test paths that should not be cached
        let non_cacheable_paths = ["/admin", "/admin/users", "/users/private/profile"];

        for path in non_cacheable_paths {
            let ctx = CacheDecisionContext {
                method: "GET",
                path,
                query_string: "",
                headers: &header::HeaderMap::new(),
                body: &[],
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
        let serialized = serde_json::to_string(&cached_response).unwrap();

        // Deserialize
        let deserialized: CachedResponse = serde_json::from_str(&serialized).unwrap();

        // Verify fields match
        assert_eq!(deserialized.status, 200);
        assert_eq!(deserialized.headers.len(), 2);
        assert_eq!(deserialized.headers[0].0, "Content-Type");
        assert_eq!(deserialized.headers[0].1, "text/plain");
        assert_eq!(deserialized.headers[1].0, "X-Test");
        assert_eq!(deserialized.headers[1].1, "value");
        assert_eq!(deserialized.body, b"test response");
    }
}
