use std::collections::HashMap;
use std::sync::Arc;

use apollo_compiler::name;
use apollo_federation::schema::ObjectFieldDefinitionPosition;
use apollo_federation::schema::ObjectOrInterfaceFieldDefinitionPosition;
use apollo_federation::schema::ObjectOrInterfaceFieldDirectivePosition;
use apollo_federation::sources::connect;
use apollo_federation::sources::connect::ConnectId;
use apollo_federation::sources::connect::JSONSelection;
use apollo_federation::sources::connect::SubSelection;
use apollo_federation::sources::source;
use tower::ServiceExt;
use tracing::Instrument;

use super::Connector;
use crate::error::Error;
use crate::error::FetchError;
use crate::graphql::Request;
use crate::http_ext;
use crate::json_ext::Object;
use crate::json_ext::Path;
use crate::json_ext::Value;
use crate::query_planner::fetch::FetchNode;
use crate::query_planner::fetch::Protocol;
use crate::query_planner::fetch::RestFetchNode;
use crate::query_planner::fetch::Variables;
use crate::query_planner::log;
use crate::query_planner::ExecutionParameters;
use crate::services::SubgraphRequest;

impl From<FetchNode> for source::query_plan::FetchNode {
    fn from(_value: FetchNode) -> source::query_plan::FetchNode {
        source::query_plan::FetchNode::Connect(connect::query_plan::FetchNode {
            source_id: ConnectId {
                label: "this_is_a_placeholder_for_now".to_string(),
                subgraph_name: "this_is_a_placeholder".into(),
                directive: ObjectOrInterfaceFieldDirectivePosition {
                    field: ObjectOrInterfaceFieldDefinitionPosition::Object(
                        ObjectFieldDefinitionPosition {
                            type_name: name!("TypeName"),
                            field_name: name!("field_name"),
                        },
                    ),
                    directive_name: name!("Directive__name"),
                    directive_index: 0,
                },
            },
            field_response_name: name!("Field"),
            field_arguments: Default::default(),
            selection: JSONSelection::Named(SubSelection {
                selections: vec![],
                star: None,
            }),
        })
    }
}

impl FetchNode {
    // TODO: let's go all in on nodestr
    pub(crate) fn update_connector_plan(
        &mut self,
        parent_service_name: &String,
        connectors: &Arc<HashMap<Arc<String>, Connector>>,
    ) {
        let as_fednext_node: source::query_plan::FetchNode = self.clone().into();

        let parent_service_name = parent_service_name.to_string();
        let connector = connectors.get(&self.service_name.to_string()).unwrap(); // TODO
                                                                                 // .map(|c| c.name().into())
                                                                                 // TODO
                                                                                 // .unwrap_or_else(|| String::new().into());
        let service_name =
            std::mem::replace(&mut self.service_name, connector.display_name().into());
        self.protocol = Arc::new(Protocol::RestFetch(RestFetchNode {
            connector_service_name: service_name.to_string(),
            connector_graph_key: connector._name(),
            parent_service_name,
        }));
        self.source_node = Some(Arc::new(as_fednext_node));
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn process_source_node<'a>(
        &'a self,
        parameters: &'a ExecutionParameters<'a>,
        data: &'a Value,
        current_dir: &'a Path,
    ) -> (Value, Vec<Error>) {
        // if self.source_node.is_some() {
        // return process_source_node(parameters, data, current_dir);
        // }
        let FetchNode {
            operation,
            operation_kind,
            operation_name,
            service_name,
            ..
        } = self;

        let Variables {
            variables,
            inverted_paths: paths,
        } = match Variables::new(
            &self.requires,
            &self.variable_usages,
            data,
            current_dir,
            // Needs the original request here
            parameters.supergraph_request,
            parameters.schema,
            &self.input_rewrites,
        ) {
            Some(variables) => variables,
            None => {
                return (Value::Object(Object::default()), Vec::new());
            }
        };

        let service_name_string = service_name.to_string();

        let (service_name, subgraph_service_name) = match &*self.protocol {
            Protocol::RestFetch(RestFetchNode {
                connector_service_name,
                parent_service_name,
                ..
            }) => (parent_service_name, connector_service_name),
            _ => (&service_name_string, &service_name_string),
        };

        let uri = parameters
            .schema
            .subgraph_url(service_name)
            .unwrap_or_else(|| {
                panic!("schema uri for subgraph '{service_name}' should already have been checked")
            })
            .clone();

        let mut subgraph_request = SubgraphRequest::builder()
            .supergraph_request(parameters.supergraph_request.clone())
            .subgraph_request(
                http_ext::Request::builder()
                    .method(http::Method::POST)
                    .uri(uri)
                    .body(
                        Request::builder()
                            .query(operation.as_serialized())
                            .and_operation_name(operation_name.as_ref().map(|n| n.to_string()))
                            .variables(variables.clone())
                            .build(),
                    )
                    .build()
                    .expect("it won't fail because the url is correct and already checked; qed"),
            )
            .subgraph_name(subgraph_service_name)
            .operation_kind(*operation_kind)
            .context(parameters.context.clone())
            .build();
        subgraph_request.query_hash = self.schema_aware_hash.clone();
        subgraph_request.authorization = self.authorization.clone();

        let service = parameters
            .service_factory
            .create(service_name)
            .expect("we already checked that the service exists during planning; qed");

        let (_parts, response) = match service
            .oneshot(subgraph_request)
            .instrument(tracing::trace_span!("subfetch_stream"))
            .await
            // TODO this is a problem since it restores details about failed service
            // when errors have been redacted in the include_subgraph_errors module.
            // Unfortunately, not easy to fix here, because at this point we don't
            // know if we should be redacting errors for this subgraph...
            .map_err(|e| match e.downcast::<FetchError>() {
                Ok(inner) => match *inner {
                    FetchError::SubrequestHttpError { .. } => *inner,
                    _ => FetchError::SubrequestHttpError {
                        status_code: None,
                        service: service_name.to_string(),
                        reason: inner.to_string(),
                    },
                },
                Err(e) => FetchError::SubrequestHttpError {
                    status_code: None,
                    service: service_name.to_string(),
                    reason: e.to_string(),
                },
            }) {
            Err(e) => {
                return (
                    Value::default(),
                    vec![e.to_graphql_error(Some(current_dir.to_owned()))],
                );
            }
            Ok(res) => res.response.into_parts(),
        };

        log::trace_subfetch(
            service_name,
            operation.as_serialized(),
            &variables,
            &response,
        );

        if !response.is_primary() {
            return (
                Value::default(),
                vec![FetchError::SubrequestUnexpectedPatchResponse {
                    service: service_name.to_string(),
                }
                .to_graphql_error(Some(current_dir.to_owned()))],
            );
        }

        let (value, errors) =
            self.response_at_path(parameters.schema, current_dir, paths, response);
        if let Some(id) = &self.id {
            if let Some(sender) = parameters.deferred_fetches.get(id.as_str()) {
                tracing::info!(monotonic_counter.apollo.router.operations.defer.fetch = 1u64);
                if let Err(e) = sender.clone().send((value.clone(), errors.clone())) {
                    tracing::error!("error sending fetch result at path {} and id {:?} for deferred response building: {}", current_dir, self.id, e);
                }
            }
        }
        (value, errors)
    }
}

#[allow(dead_code)]
fn process_source_node<'a>(
    _execution_parameters: &'a ExecutionParameters<'a>,
    _data: &'a Value,
    _current_dir: &'a Path,
) -> (Value, Vec<Error>) {
    (Default::default(), Default::default())
}

#[cfg(test)]
mod soure_node_tests {
    use apollo_compiler::NodeStr;

    use super::*;
    use crate::query_planner::fetch::SubgraphOperation;
    use crate::query_planner::PlanNode;
    use crate::services::SubgraphServiceFactory;
    use crate::spec::Query;
    use crate::spec::Schema;

    static SCHEMA: &str = r#"
        schema
          @core(feature: "https://specs.apollo.dev/core/v0.1"),
          @core(feature: "https://specs.apollo.dev/join/v0.1")
        {
          query: Query
        }

        type Query {
          me: String
        }

        directive @core(feature: String!) repeatable on SCHEMA

        directive @join__graph(name: String!, url: String!) on ENUM_VALUE
        enum join__Graph {
          ACCOUNTS @join__graph(name: "accounts" url: "http://localhost:4001/graphql")
        }
        "#;

    #[test]
    fn test_process_source_node() {
        let context = Default::default();
        let service_factory = Arc::new(SubgraphServiceFactory::empty());
        let schema = Arc::new(Schema::parse_test(SCHEMA, &Default::default()).unwrap());
        let supergraph_request = Default::default();
        let deferred_fetches = Default::default();
        let query = Arc::new(Query::empty());
        let root_node = PlanNode::Fetch(FetchNode {
            service_name: NodeStr::new("test"),
            requires: Default::default(),
            variable_usages: Default::default(),
            operation: SubgraphOperation::from_string(""),
            operation_name: Default::default(),
            operation_kind: Default::default(),
            id: Default::default(),
            input_rewrites: Default::default(),
            output_rewrites: Default::default(),
            schema_aware_hash: Default::default(),
            authorization: Default::default(),
            protocol: Default::default(),
            source_node: Default::default(),
        });
        let subscription_handle = Default::default();
        let subscription_config = Default::default();

        let execution_parameters = ExecutionParameters {
            context: &context,
            service_factory: &service_factory,
            schema: &schema,
            supergraph_request: &supergraph_request,
            deferred_fetches: &deferred_fetches,
            query: &query,
            root_node: &root_node,
            subscription_handle: &subscription_handle,
            subscription_config: &subscription_config,
        };
        let data = Default::default();
        let current_dir = Default::default();

        let expected = (Default::default(), Default::default());

        let actual = process_source_node(&execution_parameters, &data, &current_dir);

        assert_eq!(expected, actual);
    }
}