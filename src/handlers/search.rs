use bytes::Buf;
use hyper::body::aggregate;
use hyper::Response;
use hyper::{Body, StatusCode};
use tracing::*;

use toshi_types::Search;

use crate::handlers::ResponseFuture;
use crate::index::SharedCatalog;
use crate::utils::{empty_with_code, with_body};
use crate::SearchResults;

#[inline]
pub fn fold_results(results: Vec<SearchResults>) -> SearchResults {
    results.into_iter().sum()
}

pub async fn doc_search(catalog: SharedCatalog, body: Body, index: String) -> ResponseFuture {
    let span = span!(Level::INFO, "search_handler", ?index);
    let _enter = span.enter();
    let b = aggregate(body).await?;
    let req = serde_json::from_slice::<Search>(b.bytes()).unwrap();
    let c = catalog.lock().await;
    let req = if req.query.is_none() { Search::all_docs() } else { req };

    if c.exists(&index) {
        info!("Query: {:?}", req);
        //        let mut tasks = FuturesUnordered::new();
        //        tasks.push(future::Either::Left(c.search_local_index(&index, req.clone())));
        //        if c.remote_exists(&index) {
        //            tasks.push(future::Either::Right(c.search_remote_index(&index, req)));
        //        }
        //        let mut results = vec![];
        //        while let Some(Ok(r)) = tasks.next().await {
        //            results.extend(r);
        //        }
        //
        //        let response = fold_results(results);
        match c.search_local_index(&index, req.clone()).await {
            Ok(v) => Ok(with_body(v)),
            Err(e) => Ok(Response::from(e)),
        }
    } else {
        Ok(empty_with_code(StatusCode::NOT_FOUND))
    }
}

pub async fn all_docs(catalog: SharedCatalog, index: String) -> ResponseFuture {
    let body = Body::from(serde_json::to_vec(&Search::all_docs()).unwrap());
    doc_search(catalog, body, index).await
}

#[cfg(test)]
pub mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use hyper::Request;
    use pretty_assertions::assert_eq;

    use toshi_test::{cmp_float, read_body, wait_json, TestServer};
    use toshi_types::{ErrorResponse, ExactTerm, FuzzyQuery, FuzzyTerm, KeyValue, PhraseQuery, Query, TermPair};

    use crate::handlers::ResponseFuture;
    use crate::index::tests::*;
    use crate::router::Router;
    use crate::SearchResults;

    use super::*;

    type ReturnUnit = Result<(), Box<dyn std::error::Error>>;

    pub async fn run_query(req: Search, index: &str) -> ResponseFuture {
        let cat = create_test_catalog(index);
        doc_search(Arc::clone(&cat), Body::from(serde_json::to_vec(&req).unwrap()), index.into()).await
    }

    #[tokio::test]
    async fn test_term_query() -> Result<(), Box<dyn std::error::Error>> {
        let term = KeyValue::new("test_text".into(), "document".into());
        let term_query = Query::Exact(ExactTerm::new(term));
        let search = Search::new(Some(term_query), None, 10);
        let q = run_query(search, "test_index").await?;
        let body: SearchResults = wait_json(q).await;
        assert_eq!(body.hits, 3);
        Ok(())
    }

    #[tokio::test]
    async fn test_phrase_query() -> Result<(), Box<dyn std::error::Error>> {
        let terms = TermPair::new(vec!["test".into(), "document".into()], None);
        let phrase = KeyValue::new("test_text".into(), terms);
        let term_query = Query::Phrase(PhraseQuery::new(phrase));
        let search = Search::new(Some(term_query), None, 10);
        let q = run_query(search, "test_index").await?;
        let body: SearchResults = wait_json(q).await;
        assert_eq!(body.hits, 3);
        Ok(())
    }

    #[tokio::test]
    async fn test_wrong_index_error() -> ReturnUnit {
        let cat = create_test_catalog("test_index");
        let body = r#"{ "query" : { "raw": "test_text:\"document\"" } }"#;
        let (list, ts) = TestServer::new()?;
        let router = Router::new(cat, Arc::new(AtomicBool::new(false)));
        let req = Request::post(ts.uri("/asdf1234")).body(Body::from(body))?;
        let resp = ts.get(req, router.router_from_tcp(list)).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        Ok(())
    }

    #[tokio::test]
    async fn test_bad_raw_query_syntax() -> ReturnUnit {
        let cat = create_test_catalog("test_index");
        let body = r#"{ "query" : { "raw": "asd*(@sq__" } }"#;
        let err = doc_search(Arc::clone(&cat), Body::from(body), "test_index".into()).await?;
        let body: ErrorResponse = wait_json::<ErrorResponse>(err).await;
        assert_eq!(body.message, "Error in query execution: \'Syntax error in query\'");
        Ok(())
    }

    #[tokio::test]
    async fn test_unindexed_field() -> ReturnUnit {
        let cat = create_test_catalog("test_index");
        let body = r#"{ "query" : { "raw": "test_unindex:yes" } }"#;
        let r = doc_search(Arc::clone(&cat), Body::from(body), "test_index".into()).await?;
        let b = read_body(r).await?;
        let expected = "{\"message\":\"Error in query execution: 'Query on un-indexed field test_unindex'\"}";
        assert_eq!(b, expected);
        Ok(())
    }

    #[tokio::test]
    async fn test_bad_term_field_syntax() -> ReturnUnit {
        let cat = create_test_catalog("test_index");
        let body = r#"{ "query" : { "term": { "asdf": "Document" } } }"#;
        let q = doc_search(Arc::clone(&cat), Body::from(body), "test_index".into()).await?;
        let b: ErrorResponse = wait_json(q).await;
        assert_eq!(b.message, "Error in query execution: 'Unknown field: asdf'");
        Ok(())
    }

    #[tokio::test]
    async fn test_facets() -> ReturnUnit {
        let body = r#"{ "query" : { "term": { "test_text": "document" } }, "facets": { "test_facet": ["/cat"] } }"#;
        let req: Search = serde_json::from_str(body)?;
        let q = run_query(req, "test_index").await?;
        let b: SearchResults = wait_json(q).await;
        assert_eq!(b.facets[0].value, 1);
        assert_eq!(b.facets[1].value, 1);
        assert_eq!(b.facets[0].field, "/cat/cat2");
        Ok(())
    }

    #[tokio::test]
    async fn test_raw_query() -> ReturnUnit {
        let body = r#"test_text:"Duckiment""#;
        let req = Search::new(Some(Query::Raw { raw: body.into() }), None, 10);
        let q = run_query(req, "test_index").await?;
        let body: SearchResults = wait_json(q).await;
        assert_eq!(body.hits as usize, body.docs.len());
        assert_eq!(body.docs[0].doc["test_text"][0].text().unwrap(), "Test Duckiment 3");
        Ok(())
    }

    #[tokio::test]
    async fn test_fuzzy_term_query() -> ReturnUnit {
        let fuzzy = KeyValue::new("test_text".into(), FuzzyTerm::new("document".into(), 0, false));
        let term_query = Query::Fuzzy(FuzzyQuery::new(fuzzy));
        let search = Search::new(Some(term_query), None, 10);
        let q = run_query(search, "test_index").await?;
        let body: SearchResults = wait_json(q).await;

        assert_eq!(body.hits as usize, body.docs.len());
        assert_eq!(body.hits, 3);
        assert_eq!(body.docs.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn test_inclusive_range_query() -> ReturnUnit {
        let body = r#"{ "query" : { "range" : { "test_i64" : { "gte" : 2012, "lte" : 2015 } } } }"#;
        let req: Search = serde_json::from_str(body)?;
        let q = run_query(req, "test_index").await?;
        let body: SearchResults = wait_json(q).await;
        assert_eq!(body.hits as usize, body.docs.len());
        assert_eq!(cmp_float(body.docs[0].score.unwrap(), 1.0), true);
        Ok(())
    }

    #[tokio::test]
    async fn test_exclusive_range_query() -> ReturnUnit {
        let body = r#"{ "query" : { "range" : { "test_i64" : { "gt" : 2012, "lt" : 2015 } } } }"#;
        let req: Search = serde_json::from_str(&body)?;
        let q = run_query(req, "test_index").await?;
        let body: SearchResults = wait_json(q).await;
        assert_eq!(body.hits as usize, body.docs.len());
        assert_eq!(cmp_float(body.docs[0].score.unwrap(), 1.0), true);
        Ok(())
    }

    #[tokio::test]
    async fn test_regex_query() -> ReturnUnit {
        let body = r#"{ "query" : { "regex" : { "test_text" : "d[ou]{1}c[k]?ument" } } }"#;
        let req: Search = serde_json::from_str(&body)?;
        let q = run_query(req, "test_index").await?;
        let body: SearchResults = wait_json(q).await;
        assert_eq!(body.hits, 4);
        Ok(())
    }

    #[tokio::test]
    async fn test_bool_query() -> ReturnUnit {
        let test_json = r#"{"query": { "bool": {
                "must": [ { "term": { "test_text": "document" } } ],
                "must_not": [ {"range": {"test_i64": { "gt": 2017 } } } ] } } }"#;

        let query = serde_json::from_str::<Search>(test_json)?;
        let q = run_query(query, "test_index").await?;
        let body: SearchResults = wait_json(q).await;
        assert_eq!(body.hits, 2);
        Ok(())
    }
}
