# Actix Request-Reply Cache

[![Crates.io](https://img.shields.io/crates/v/actix-request-reply-cache.svg)](https://crates.io/crates/actix-request-reply-cache)
[![Documentation](https://docs.rs/actix-request-reply-cache/badge.svg)](https://docs.rs/actix-request-reply-cache)
[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

A Redis-backed response caching middleware for Actix Web applications.

## Features

- **Redis-backed caching**: Store HTTP responses in Redis for fast retrieval
- **Configurable TTL**: Set custom expiration times for cached responses
- **Size limits**: Control maximum response size to cache
- **Flexible cache control**: Use predicate functions to determine what should be cached
- **Cache key customization**: Set your own cache key prefix
- **Standard compliant**: Respects HTTP Cache-Control headers
- **Minimal performance impact**: Fast cache lookup and non-blocking I/O

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
actix-request-reply-cache = "0.1.0"
```

## Basic Usage

```rust
use actix_web::{web, App, HttpServer};
use actix_request_reply_cache::RedisCacheMiddlewareBuilder;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Create the cache middleware with default settings
    let cache = RedisCacheMiddlewareBuilder::new("redis://127.0.0.1:6379")
        .build();
        
    HttpServer::new(move || {
        App::new()
            .wrap(cache.clone())
            .service(web::resource("/").to(|| async { "Hello world!" }))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}
```

## Advanced Configuration

```rust
use actix_web::{web, App, HttpServer};
use actix_request_reply_cache::RedisCacheMiddlewareBuilder;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Create a more customized cache middleware
    let cache = RedisCacheMiddlewareBuilder::new("redis://127.0.0.1:6379")
        .ttl(300)  // Cache for 5 minutes
        .max_cacheable_size(512 * 1024)  // Only cache responses <= 512KB
        .cache_prefix("my-api-cache:")  // Custom Redis key prefix
        .cache_if(|ctx| {
            // Only cache GET requests
            if ctx.method != "GET" {
                return false;
            }
            
            // Don't cache if Authorization header is present
            if ctx.headers.contains_key("Authorization") {
                return false;
            }
            
            // Don't cache admin routes
            if ctx.path.starts_with("/admin") {
                return false;
            }
            
            true
        })
        .build()
        .await;
        
    HttpServer::new(move || {
        App::new()
            .wrap(cache.clone())
            .service(web::resource("/api/users").to(get_users))
            .service(web::resource("/api/products").to(get_products))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}
```

## Cache Decision Context

The `cache_if` predicate receives a `CacheDecisionContext` with the following information:

- `method`: HTTP method string ("GET", "POST", etc.)
- `path`: Request path
- `query_string`: URL query parameters
- `headers`: HTTP request headers
- `body`: Request body as bytes

This allows for fine-grained control over what gets cached.

## How It Works

1. Incoming requests are intercepted by the middleware
2. If Cache-Control headers specify no-cache/no-store, the request is processed normally
3. Otherwise, a cache key is computed based on the request method, path, query string and body
4. If a matching response is found in Redis, it's returned immediately with an X-Cache: HIT header
5. If no match is found, the request is processed normally, and if the response is successful:
   - The response is serialized and stored in Redis with the configured TTL
   - The original response is returned to the client

## License

This project is licensed under the [MIT License](./LICENSE). 