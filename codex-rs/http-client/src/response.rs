//! HTTP responses whose body errors are reported as [`HttpError`].

use std::ops::Deref;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use serde::de::DeserializeOwned;

use crate::HttpError;

#[derive(Debug)]
pub struct HttpResponse {
    inner: reqwest::Response,
}

impl HttpResponse {
    /// Preserves HTTP frames, including gRPC trailers.
    pub fn into_http_response(
        self,
    ) -> http::Response<impl http_body::Body<Data = Bytes, Error = HttpError> + Send> {
        let response: http::Response<reqwest::Body> = self.inner.into();
        response.map(|body| MappedBody {
            body: Box::pin(body),
        })
    }

    pub async fn bytes(self) -> Result<Bytes, HttpError> {
        Ok(self.inner.bytes().await?)
    }

    pub async fn text(self) -> Result<String, HttpError> {
        Ok(self.inner.text().await?)
    }

    pub async fn json<T: DeserializeOwned>(self) -> Result<T, HttpError> {
        Ok(self.inner.json().await?)
    }

    pub async fn chunk(&mut self) -> Result<Option<Bytes>, HttpError> {
        Ok(self.inner.chunk().await?)
    }

    pub fn error_for_status(self) -> Result<Self, HttpError> {
        Ok(Self {
            inner: self.inner.error_for_status()?,
        })
    }

    pub fn error_for_status_ref(&self) -> Result<&Self, HttpError> {
        self.inner.error_for_status_ref()?;
        Ok(self)
    }

    pub fn bytes_stream(self) -> impl Stream<Item = Result<Bytes, HttpError>> + Send + Unpin {
        Box::pin(
            self.inner
                .bytes_stream()
                .map(|result| result.map_err(HttpError::from)),
        )
    }
}

struct MappedBody<B> {
    body: Pin<Box<B>>,
}

impl<B: http_body::Body<Data = Bytes, Error = reqwest::Error>> http_body::Body for MappedBody<B> {
    type Data = Bytes;
    type Error = HttpError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, HttpError>>> {
        self.get_mut()
            .body
            .as_mut()
            .poll_frame(cx)
            .map(|frame| frame.map(|frame| frame.map_err(HttpError::from)))
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

impl Deref for HttpResponse {
    type Target = reqwest::Response;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl From<reqwest::Response> for HttpResponse {
    fn from(inner: reqwest::Response) -> Self {
        Self { inner }
    }
}

impl<T: Into<reqwest::Body>> From<http::Response<T>> for HttpResponse {
    fn from(response: http::Response<T>) -> Self {
        reqwest::Response::from(response).into()
    }
}
