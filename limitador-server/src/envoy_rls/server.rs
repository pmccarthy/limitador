use crate::envoy_rls::server::envoy::service::ratelimit::v3::rate_limit_response::Code;
use crate::envoy_rls::server::envoy::service::ratelimit::v3::rate_limit_service_server::{
    RateLimitService, RateLimitServiceServer,
};
use crate::envoy_rls::server::envoy::service::ratelimit::v3::{
    RateLimitRequest, RateLimitResponse,
};
use crate::Limiter;
use std::collections::HashMap;
use std::sync::Arc;
use tonic::{transport, transport::Server, Request, Response, Status};

include!("envoy_types.rs");

pub struct MyRateLimiter {
    limiter: Arc<Limiter>,
}

impl MyRateLimiter {
    pub fn new(limiter: Arc<Limiter>) -> MyRateLimiter {
        MyRateLimiter { limiter }
    }
}

#[tonic::async_trait]
impl RateLimitService for MyRateLimiter {
    async fn should_rate_limit(
        &self,
        request: Request<RateLimitRequest>,
    ) -> Result<Response<RateLimitResponse>, Status> {
        debug!("Request received: {:?}", request);

        let mut values: HashMap<String, String> = HashMap::new();
        let req = request.into_inner();
        let namespace = req.domain;

        if namespace.is_empty() {
            return Ok(Response::new(RateLimitResponse {
                overall_code: Code::Unknown.into(),
                statuses: vec![],
                request_headers_to_add: vec![],
                response_headers_to_add: vec![],
            }));
        }

        for descriptor in &req.descriptors {
            for entry in &descriptor.entries {
                values.insert(entry.key.clone(), entry.value.clone());
            }
        }

        // "hits_addend" is optional according to the spec, and should default
        // to 1, However, with the autogenerated structs it defaults to 0.
        let hits_addend = if req.hits_addend == 0 {
            1
        } else {
            req.hits_addend
        };

        let is_rate_limited_res = match &*self.limiter {
            Limiter::Blocking(limiter) => {
                limiter.check_rate_limited_and_update(namespace, &values, i64::from(hits_addend))
            }
            Limiter::Async(limiter) => {
                limiter
                    .check_rate_limited_and_update(namespace, &values, i64::from(hits_addend))
                    .await
            }
        };

        let resp_code = match is_rate_limited_res {
            Ok(rate_limited) => {
                if rate_limited {
                    Code::OverLimit
                } else {
                    Code::Ok
                }
            }
            Err(e) => {
                // In this case we could return "Code::Unknown" but that's not
                // very helpful. When envoy receives "Unknown" it simply lets
                // the request pass and this cannot be configured using the
                // "failure_mode_deny" attribute, so it's equivalent to
                // returning "Code::Ok". That's why we return an "unavailable"
                // error here. What envoy does after receiving that kind of
                // error can be configured with "failure_mode_deny". The only
                // errors that can happen here have to do with connecting to the
                // limits storage, which should be temporary.
                error!("Error: {:?}", e);
                return Err(Status::unavailable("Service unavailable"));
            }
        };

        let reply = RateLimitResponse {
            overall_code: resp_code.into(),
            statuses: vec![],
            request_headers_to_add: vec![],
            response_headers_to_add: vec![],
        };

        Ok(Response::new(reply))
    }
}

pub async fn run_envoy_rls_server(
    address: String,
    limiter: Arc<Limiter>,
) -> Result<(), transport::Error> {
    let rate_limiter = MyRateLimiter::new(limiter);
    let svc = RateLimitServiceServer::new(rate_limiter);

    Server::builder()
        .add_service(svc)
        .serve(address.parse().unwrap())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envoy_rls::server::envoy::extensions::common::ratelimit::v3::rate_limit_descriptor::Entry;
    use crate::envoy_rls::server::envoy::extensions::common::ratelimit::v3::RateLimitDescriptor;
    use limitador::limit::Limit;
    use limitador::RateLimiter;
    use tonic::IntoRequest;

    // All these tests use the in-memory storage implementation to simplify. We
    // know that some storage implementations like the Redis one trade
    // rate-limiting accuracy for performance. That would be a bit more
    // complicated to test.
    // Also, the logic behind these endpoints is well tested in the library,
    // that's why running some simple tests here should be enough.

    #[tokio::test]
    async fn test_returns_ok_and_overlimit_correctly() {
        let namespace = "test_namespace";
        let limit = Limit::new(namespace, 1, 60, vec!["req.method == GET"], vec!["app_id"]);

        let limiter = RateLimiter::default();
        limiter.add_limit(&limit).unwrap();

        let rate_limiter = MyRateLimiter::new(Arc::new(Limiter::Blocking(limiter)));

        let req = RateLimitRequest {
            domain: namespace.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![
                    Entry {
                        key: "req.method".to_string(),
                        value: "GET".to_string(),
                    },
                    Entry {
                        key: "app_id".to_string(),
                        value: "1".to_string(),
                    },
                ],
            }],
            hits_addend: 1,
        };

        // There's a limit of 1, so the first request should return "OK" and the
        // second "OverLimit".

        assert_eq!(
            rate_limiter
                .should_rate_limit(req.clone().into_request())
                .await
                .unwrap()
                .into_inner()
                .overall_code,
            i32::from(Code::Ok)
        );

        assert_eq!(
            rate_limiter
                .should_rate_limit(req.clone().into_request())
                .await
                .unwrap()
                .into_inner()
                .overall_code,
            i32::from(Code::OverLimit)
        );
    }

    #[tokio::test]
    async fn test_returns_ok_when_no_limits_apply() {
        // No limits saved
        let rate_limiter = MyRateLimiter::new(Arc::new(Limiter::new().await.unwrap()));

        let req = RateLimitRequest {
            domain: "test_namespace".to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "req.method".to_string(),
                    value: "GET".to_string(),
                }],
            }],
            hits_addend: 1,
        }
        .into_request();

        assert_eq!(
            rate_limiter
                .should_rate_limit(req)
                .await
                .unwrap()
                .into_inner()
                .overall_code,
            i32::from(Code::Ok)
        );
    }

    #[tokio::test]
    async fn test_returns_unknown_when_domain_is_empty() {
        let rate_limiter = MyRateLimiter::new(Arc::new(Limiter::new().await.unwrap()));

        let req = RateLimitRequest {
            domain: "".to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![Entry {
                    key: "req.method".to_string(),
                    value: "GET".to_string(),
                }],
            }],
            hits_addend: 1,
        }
        .into_request();

        assert_eq!(
            rate_limiter
                .should_rate_limit(req)
                .await
                .unwrap()
                .into_inner()
                .overall_code,
            i32::from(Code::Unknown)
        );
    }

    #[tokio::test]
    async fn test_takes_into_account_all_the_descriptors() {
        let limiter = RateLimiter::default();

        let namespace = "test_namespace";

        vec![
            Limit::new(namespace, 10, 60, vec!["x == 1"], vec!["z"]),
            Limit::new(namespace, 0, 60, vec!["x == 1", "y == 2"], vec!["z"]),
        ]
        .iter()
        .for_each(|limit| limiter.add_limit(&limit).unwrap());

        let rate_limiter = MyRateLimiter::new(Arc::new(Limiter::Blocking(limiter)));

        let req = RateLimitRequest {
            domain: namespace.to_string(),
            descriptors: vec![
                RateLimitDescriptor {
                    entries: vec![
                        Entry {
                            key: "x".to_string(),
                            value: "1".to_string(),
                        },
                        Entry {
                            key: "z".to_string(),
                            value: "1".to_string(),
                        },
                    ],
                },
                // If this is taken into account, the result will be "overlimit"
                // because of the second limit that has a max of 0.
                RateLimitDescriptor {
                    entries: vec![Entry {
                        key: "y".to_string(),
                        value: "2".to_string(),
                    }],
                },
            ],
            hits_addend: 1,
        };

        assert_eq!(
            rate_limiter
                .should_rate_limit(req.clone().into_request())
                .await
                .unwrap()
                .into_inner()
                .overall_code,
            i32::from(Code::OverLimit)
        );
    }

    #[tokio::test]
    async fn test_takes_into_account_the_hits_addend_param() {
        let namespace = "test_namespace";
        let limit = Limit::new(namespace, 10, 60, vec!["x == 1"], vec!["y"]);

        let limiter = RateLimiter::default();
        limiter.add_limit(&limit).unwrap();

        let rate_limiter = MyRateLimiter::new(Arc::new(Limiter::Blocking(limiter)));

        let req = RateLimitRequest {
            domain: namespace.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![
                    Entry {
                        key: "x".to_string(),
                        value: "1".to_string(),
                    },
                    Entry {
                        key: "y".to_string(),
                        value: "1".to_string(),
                    },
                ],
            }],
            hits_addend: 6,
        };

        // There's a limit of 10, "hits_addend" is 6, so the first request
        // should return "Ok" and the second "OverLimit".

        assert_eq!(
            rate_limiter
                .should_rate_limit(req.clone().into_request())
                .await
                .unwrap()
                .into_inner()
                .overall_code,
            i32::from(Code::Ok)
        );

        assert_eq!(
            rate_limiter
                .should_rate_limit(req.clone().into_request())
                .await
                .unwrap()
                .into_inner()
                .overall_code,
            i32::from(Code::OverLimit)
        );
    }

    #[tokio::test]
    async fn test_0_hits_addend_is_converted_to_1() {
        // "hits_addend" is optional according to the spec, and should default
        // to 1, However, with the autogenerated structs it defaults to 0.
        let namespace = "test_namespace";
        let limit = Limit::new(namespace, 1, 60, vec!["x == 1"], vec!["y"]);

        let limiter = RateLimiter::default();
        limiter.add_limit(&limit).unwrap();

        let rate_limiter = MyRateLimiter::new(Arc::new(Limiter::Blocking(limiter)));

        let req = RateLimitRequest {
            domain: namespace.to_string(),
            descriptors: vec![RateLimitDescriptor {
                entries: vec![
                    Entry {
                        key: "x".to_string(),
                        value: "1".to_string(),
                    },
                    Entry {
                        key: "y".to_string(),
                        value: "2".to_string(),
                    },
                ],
            }],
            hits_addend: 0,
        };

        // There's a limit of 1, and hits_addend is converted to 1, so the first
        // request should return "OK" and the second "OverLimit".

        assert_eq!(
            rate_limiter
                .should_rate_limit(req.clone().into_request())
                .await
                .unwrap()
                .into_inner()
                .overall_code,
            i32::from(Code::Ok)
        );

        assert_eq!(
            rate_limiter
                .should_rate_limit(req.clone().into_request())
                .await
                .unwrap()
                .into_inner()
                .overall_code,
            i32::from(Code::OverLimit)
        );
    }
}
