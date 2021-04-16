use {
    anyhow::Result,
    constants::{
        STAT_REQUEST_RECEIVED, STAT_SIGNATURE_VALIDATION_FAILED, STAT_SIGNATURE_VALIDATION_SUCCESS,
    },
    logger,
    rsa::RSAPublicKey,
    signature_verifier,
    std::{borrow::Cow, sync::Arc},
    types::Config,
    warp::{http::StatusCode, reply, Filter},
};

pub async fn listen<S: Send + Sync + Clone + 'static>(
    config_ref: Arc<Config>,
    public_key_ref: Arc<RSAPublicKey>,
    state: S,
    handler: impl Fn(S) -> Result<()> + Clone + Send + Sync + 'static,
) {
    let refresher = refresher(config_ref, public_key_ref, state, handler);
    warp::serve(refresher).run(([127, 0, 0, 1], 8090)).await;
}

fn refresher<S: Send + Sync + Clone + 'static>(
    config_ref: Arc<Config>,
    public_key_ref: Arc<RSAPublicKey>,
    state: S,
    handler: impl Fn(S) -> Result<()> + Clone + Send + Sync + 'static,
) -> impl Filter<Extract = (reply::WithStatus<Cow<'static, str>>,), Error = warp::Rejection> + Clone
{
    warp::post()
        .and(warp::path("refresh"))
        .and(validate_payload_and_signature(config_ref, public_key_ref))
        .map(move |res: types::ValidationResult| {
            logger::success(STAT_SIGNATURE_VALIDATION_SUCCESS);

            let state = state.clone();
            let body = res.body;
            match handler(state) {
                Ok(()) => reply::with_status(Cow::from(body), StatusCode::OK),
                Err(e) => {
                    let e = e.to_string();
                    logger::error(&e);

                    reply::with_status(Cow::from(e), StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        })
}

fn validate_payload_and_signature(
    config_ref: Arc<Config>,
    public_key_ref: Arc<RSAPublicKey>,
) -> impl Filter<Extract = (types::ValidationResult,), Error = warp::Rejection> + Clone {
    warp::body::bytes().and(warp::header("Signature")).and_then(
        move |payload: warp::hyper::body::Bytes, signature: String| {
            let config = config_ref.clone();
            let public_key = public_key_ref.clone();

            async move {
                logger::info(STAT_REQUEST_RECEIVED);

                match signature_verifier::validate_payload_and_signature(
                    &payload.to_vec(),
                    &signature,
                    &config,
                    &public_key,
                ) {
                    Ok(res) => Ok(res),
                    Err(e) => {
                        logger::warn(STAT_SIGNATURE_VALIDATION_FAILED);
                        Err(warp::reject::custom(e))
                    }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        reqwest::StatusCode,
        rsa::{hash::Hash, PaddingScheme, RSAPrivateKey},
        serde_json,
        sha2::{Digest, Sha256},
        std::time::{SystemTime, UNIX_EPOCH},
        types::Payload,
    };

    #[tokio::test]
    async fn passing_request() {
        const TEST_ENDPOINT: &str = "https://endpoint.example.com/";
        let (pub_key, priv_key) = new_keypair();

        tokio::spawn(run_server(TEST_ENDPOINT.to_owned(), pub_key));
        let request_thread = tokio::spawn(make_request(TEST_ENDPOINT.to_owned(), priv_key));
        assert_eq!(
            request_thread.await.expect("HTTP request failed").status(),
            StatusCode::OK,
            "Response not OK"
        );
    }

    #[tokio::test]
    async fn failing_request() {
        const TEST_ENDPOINT: &str = "https://endpoint.example.com/";
        const BAD_ENDPOINT: &str = "https://endpoint.bad-example.com/";
        let (pub_key, priv_key) = new_keypair();

        tokio::spawn(run_server(TEST_ENDPOINT.to_owned(), pub_key));
        let request_thread = tokio::spawn(make_request(BAD_ENDPOINT.to_owned(), priv_key));

        assert_ne!(
            request_thread.await.expect("HTTP request failed").status(),
            StatusCode::OK,
            "Response is OK"
        );
    }

    async fn run_server(endpoint: String, pub_key: RSAPublicKey) {
        listen(
            Arc::new(Config { endpoint }),
            Arc::new(pub_key),
            (),
            move |_| Ok(()),
        )
        .await;
    }

    async fn make_request(endpoint: String, priv_key: RSAPrivateKey) -> reqwest::Response {
        let payload = serde_json::to_string(&Payload {
            timestamp: now_ms(),
            endpoint,
        })
        .unwrap();

        let signature = sign_and_hash(&payload, &priv_key);

        let client = reqwest::Client::new();
        client
            .post("http://localhost:8090/refresh")
            .body(payload)
            .header("Signature", signature)
            .send()
            .await
            .expect("Failed to complete HTTP request")
    }

    // Helpers

    fn new_keypair() -> (RSAPublicKey, RSAPrivateKey) {
        use rand::rngs::OsRng;
        let mut rng = OsRng;
        let bits = 2048;
        let private_key =
            RSAPrivateKey::new(&mut rng, bits).expect("Failed to generate private key");
        let public_key = RSAPublicKey::from(&private_key);
        (public_key, private_key)
    }

    fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
    }

    fn sign_and_hash(body: &str, priv_key: &RSAPrivateKey) -> String {
        base64::encode(
            priv_key
                .sign(
                    PaddingScheme::PKCS1v15Sign {
                        hash: Some(Hash::SHA2_256),
                    },
                    &hash(body),
                )
                .expect("signing request failed"),
        )
    }

    fn hash(body: &str) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(body);
        Vec::from(hasher.finalize().as_slice())
    }
}
