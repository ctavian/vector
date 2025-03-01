mod auth;
pub mod region;

//TODO: replace with direct import
pub use super::auth::AwsAuthentication;
use crate::config::ProxyConfig;
use crate::{http::HttpError, tls::MaybeTlsSettings};
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use http::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Method, Request, Response, StatusCode,
};
use hyper::body::{Body, HttpBody};
use hyper::client;
use once_cell::sync::OnceCell;
use regex::bytes::RegexSet;
pub use region::{region_from_endpoint, RegionOrEndpoint};
use rusoto_core::credential::ProfileProvider;
use rusoto_core::{
    request::{
        DispatchSignedRequest, DispatchSignedRequestFuture, HttpDispatchError, HttpResponse,
    },
    ByteStream, Region, RusotoError,
};
use rusoto_credential::{
    AutoRefreshingProvider, AwsCredentials, ChainProvider, CredentialsError, ProvideAwsCredentials,
    StaticProvider,
};
use rusoto_signature::{SignedRequest, SignedRequestPayload};
use rusoto_sts::{StsAssumeRoleSessionCredentialsProvider, StsClient, WebIdentityProvider};
use snafu::{ResultExt, Snafu};
use std::{
    fmt, io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tower::{Service, ServiceExt};
// use crate::http;

pub type Client = HttpClient<crate::http::HttpClient<RusotoBody>>;

pub fn client(proxy: &ProxyConfig) -> crate::Result<Client> {
    let settings = MaybeTlsSettings::enable_client()?;
    let client = crate::http::HttpClient::new(settings, proxy)?;
    Ok(HttpClient { client })
}

pub fn custom_client(
    proxy: &ProxyConfig,
    client_builder: &mut client::Builder,
) -> crate::Result<Client> {
    let settings = MaybeTlsSettings::enable_client()?;
    let client = crate::http::HttpClient::new_with_custom_client(settings, proxy, client_builder)?;
    Ok(HttpClient { client })
}

#[derive(Debug, Snafu)]
enum AwsRusotoError {
    #[snafu(display("Failed to create request dispatcher"))]
    DispatcherError,

    #[snafu(display("Invalid AWS credentials: {}", source))]
    InvalidAwsCredentials { source: CredentialsError },
}

// A custom chain provider incorporating web identity support
// See - https://github.com/rusoto/rusoto/issues/1781
pub struct CustomChainProvider {
    chain_provider: ChainProvider,
    web_provider: WebIdentityProvider,
}

impl CustomChainProvider {
    pub fn new() -> CustomChainProvider {
        CustomChainProvider {
            chain_provider: ChainProvider::new(),
            web_provider: WebIdentityProvider::from_k8s_env(),
        }
    }

    pub fn set_timeout(&mut self, duration: Duration) {
        self.chain_provider.set_timeout(duration);
    }
}

impl Default for CustomChainProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ProvideAwsCredentials for CustomChainProvider {
    async fn credentials(&self) -> Result<AwsCredentials, CredentialsError> {
        match self.web_provider.credentials().await {
            Ok(creds) => Ok(creds),
            Err(error_1) => match self.chain_provider.credentials().await {
                Ok(creds) => Ok(creds),
                Err(error_2) => Err(CredentialsError::new(format!(
                    "Failed creating AWS credentials. Errors: {:?}",
                    [error_1, error_2]
                ))),
            },
        }
    }
}

// A place-holder for the types of AWS credentials we support
pub enum AwsCredentialsProvider {
    Default(AutoRefreshingProvider<CustomChainProvider>),
    Role(AutoRefreshingProvider<StsAssumeRoleSessionCredentialsProvider>),
    Static(StaticProvider),
    File(AutoRefreshingProvider<ProfileProvider>),
}

impl fmt::Debug for AwsCredentialsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Default(_) => "default",
            Self::Role(_) => "role",
            Self::Static(_) => "static",
            Self::File(_) => "file",
        };

        f.debug_tuple("AwsCredentialsProvider")
            .field(&name)
            .finish()
    }
}

impl AwsCredentialsProvider {
    pub fn new(region: &Region, assume_role: Option<String>) -> crate::Result<Self> {
        if let Some(role) = assume_role {
            debug!("Using STS assume role credentials for AWS.");

            let dispatcher = rusoto_core::request::HttpClient::new()
                .map_err(|_| AwsRusotoError::DispatcherError)?;

            let mut credentials = CustomChainProvider::new();
            credentials.set_timeout(Duration::from_secs(8));

            let sts = StsClient::new_with(dispatcher, credentials, region.clone());

            let provider = StsAssumeRoleSessionCredentialsProvider::new(
                sts,
                role,
                "default".to_owned(),
                None,
                None,
                None,
                None,
            );

            let creds = AutoRefreshingProvider::new(provider).context(InvalidAwsCredentials)?;
            Ok(Self::Role(creds))
        } else {
            debug!("Using default credentials provider for AWS.");
            let mut chain = CustomChainProvider::new();
            // 8 seconds because our default healthcheck timeout
            // is 10 seconds.
            chain.set_timeout(Duration::from_secs(8));

            let creds = AutoRefreshingProvider::new(chain).context(InvalidAwsCredentials)?;

            Ok(Self::Default(creds))
        }
    }

    pub fn new_minimal<A: Into<String>, S: Into<String>>(access_key: A, secret_key: S) -> Self {
        Self::Static(StaticProvider::new_minimal(
            access_key.into(),
            secret_key.into(),
        ))
    }

    pub fn new_with_credentials_file(credentials_file: &str, profile: &str) -> crate::Result<Self> {
        let creds = AutoRefreshingProvider::new(ProfileProvider::with_configuration(
            credentials_file,
            profile,
        ))
        .context(InvalidAwsCredentials)?;
        Ok(Self::File(creds))
    }
}

#[async_trait]
impl ProvideAwsCredentials for AwsCredentialsProvider {
    async fn credentials(&self) -> Result<AwsCredentials, CredentialsError> {
        let fut = match self {
            Self::Default(p) => p.credentials(),
            Self::Role(p) => p.credentials(),
            Self::Static(p) => p.credentials(),
            Self::File(p) => p.credentials(),
        };
        fut.await
    }
}

#[derive(Debug, Clone)]
pub struct HttpClient<T> {
    client: T,
}

#[derive(Debug)]
pub struct RusotoBody {
    inner: Option<SignedRequestPayload>,
}

impl<T> HttpClient<T> {
    pub const fn new(client: T) -> Self {
        HttpClient { client }
    }
}

impl<T> DispatchSignedRequest for HttpClient<T>
where
    T: Service<Request<RusotoBody>, Response = Response<Body>, Error = HttpError>
        + Clone
        + Send
        + 'static,
    T::Future: Send + 'static,
{
    // Adaptation of https://docs.rs/rusoto_core/0.44.0/src/rusoto_core/request.rs.html#314-416
    fn dispatch(
        &self,
        request: SignedRequest,
        timeout: Option<Duration>,
    ) -> DispatchSignedRequestFuture {
        assert!(timeout.is_none(), "timeout is not supported at this level");

        let client = self.client.clone();

        Box::pin(async move {
            let method = match request.method() {
                "POST" => Method::POST,
                "PUT" => Method::PUT,
                "DELETE" => Method::DELETE,
                "GET" => Method::GET,
                "HEAD" => Method::HEAD,
                v => {
                    return Err(HttpDispatchError::new(format!(
                        "Unsupported HTTP verb {}",
                        v
                    )));
                }
            };

            let mut headers = HeaderMap::new();
            for h in request.headers().iter() {
                let header_name = match h.0.parse::<HeaderName>() {
                    Ok(name) => name,
                    Err(err) => {
                        return Err(HttpDispatchError::new(format!(
                            "Error parsing header name: {}",
                            err
                        )));
                    }
                };
                for v in h.1.iter() {
                    let header_value = match HeaderValue::from_bytes(v) {
                        Ok(value) => value,
                        Err(err) => {
                            return Err(HttpDispatchError::new(format!(
                                "Value of header {:?} contains invalid header byte. Error: {}",
                                h.0, err
                            )));
                        }
                    };
                    headers.append(&header_name, header_value);
                }
            }

            let mut uri = format!(
                "{}://{}{}",
                request.scheme(),
                request.hostname(),
                request.canonical_path()
            );

            if !request.canonical_query_string().is_empty() {
                uri += &format!("?{}", request.canonical_query_string());
            }

            let mut request = Request::builder()
                .method(method)
                .uri(uri)
                .body(RusotoBody::from(request.payload))
                .map_err(|error| format!("Error building request: {}", error))
                .map_err(HttpDispatchError::new)?;

            *request.headers_mut() = headers;

            let response = client.oneshot(request).await.map_err(|error| {
                HttpDispatchError::new(format!("Error during dispatch: {}", error))
            })?;

            let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
            let headers = response
                .headers()
                .iter()
                .map(|(h, v)| {
                    let value_string = v.to_str().unwrap().to_owned();
                    // This should always be valid since we are coming from http.
                    let name = HeaderName::from_bytes(h.as_ref())
                        .expect("Should always be a valid header");
                    (name, value_string)
                })
                .collect();

            let body = response
                .into_body()
                .map(|try_chunk| try_chunk.map_err(|e| io::Error::new(io::ErrorKind::Other, e)));

            Ok(HttpResponse {
                status,
                headers,
                body: ByteStream::new(body),
            })
        })
    }
}

impl HttpBody for RusotoBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_data(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        match self.inner.as_mut() {
            Some(SignedRequestPayload::Buffer(buf)) => {
                if !buf.is_empty() {
                    let buf = buf.split_off(0);
                    Poll::Ready(Some(Ok(buf)))
                } else {
                    Poll::Ready(None)
                }
            }
            Some(SignedRequestPayload::Stream(stream)) => match stream.poll_next_unpin(cx) {
                Poll::Ready(Some(result)) => match result {
                    Ok(buf) => Poll::Ready(Some(Ok(buf))),
                    Err(error) => Poll::Ready(Some(Err(error))),
                },
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            },
            None => Poll::Ready(None),
        }
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        Poll::Ready(Ok(None))
    }
}

impl From<Option<SignedRequestPayload>> for RusotoBody {
    fn from(inner: Option<SignedRequestPayload>) -> Self {
        RusotoBody { inner }
    }
}

static RETRIABLE_CODES: OnceCell<RegexSet> = OnceCell::new();

pub fn is_retriable_error<T>(error: &RusotoError<T>) -> bool {
    match error {
        RusotoError::HttpDispatch(_) => true,
        RusotoError::Unknown(response) => {
            // This header is a direct indication that we should retry the request. Eventually it'd
            // be nice to actually schedule the retry after the given delay, but for now we just
            // check that it contains a positive value.
            let retry_header = response
                .headers
                .get("x-amz-retry-after")
                .and_then(|value| value.parse::<isize>().ok())
                .filter(|duration| *duration > 0);

            // Certain 400-level responses will contain an error code indicating that the request
            // should be retried. Since we don't retry 400-level responses by default, we'll look
            // for these specifically before falling back to more general heuristics. Because AWS
            // services use a mix of XML and JSON response bodies and Rusoto doesn't give us
            // a parsed representation, we resort to a simple string match.
            //
            // S3: RequestTimeout
            // SQS: RequestExpired, ThrottlingException
            // ECS: RequestExpired, ThrottlingException
            // Kinesis: RequestExpired, ThrottlingException
            // Cloudwatch: RequestExpired, ThrottlingException
            //
            // Now just look for those when it's a client_error
            let re = RETRIABLE_CODES.get_or_init(|| {
                RegexSet::new(&["RequestTimeout", "RequestExpired", "ThrottlingException"])
                    .expect("invalid regex")
            });

            retry_header.is_some()
                || response.status.is_server_error()
                || response.status == http::StatusCode::TOO_MANY_REQUESTS
                || (response.status.is_client_error() && re.is_match(&response.body))
        }
        _ => false,
    }
}
