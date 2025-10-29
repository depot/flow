use anyhow::Context;

use crate::catalog::{DataPlaneSelector, NameSelector, SpecTypeSelector};
use crate::{auth, graphql::*, output};

#[derive(Default, Debug, Clone, clap::Args)]
#[clap(rename_all = "kebab-case")]
pub struct List {
    /// Include "Reads From" / "Writes To" columns in the output.
    #[clap(short = 'f', long = "flows")]
    pub include_flows: bool,
    /// Include the models in the output (requires '--output json|yaml')
    #[clap(long = "models")]
    pub include_models: bool,
    #[clap(flatten)]
    pub name_selector: NameSelector,
    #[clap(flatten)]
    pub type_selector: SpecTypeSelector,
    #[clap(flatten)]
    pub data_plane_selector: DataPlaneSelector,

    /// This option is not exposed as a CLI argument. It just allows us to skip
    /// fetching publication info in contexts where it's not necessary.
    #[clap(skip = true)]
    pub include_last_publication: bool,
}

#[derive(graphql_client::GraphQLQuery)]
#[graphql(
    schema_path = "../flow-client/control-plane-api.graphql",
    query_path = "src/catalog/list/query.graphql",
    response_derives = "Serialize,Clone",
    variables_derives = "Clone",
    extern_enums("CatalogType")
)]
struct ListLiveSpecsQuery;

pub async fn do_list(ctx: &mut crate::CliContext, list_args: &List) -> anyhow::Result<()> {
    if list_args.include_models && ctx.get_output_type() == output::OutputType::Table {
        anyhow::bail!(
            "cannot output models as a table, must pass `--output json` or `--output yaml`"
        );
    }
    let rows = fetch_live_specs(ctx, list_args.clone()).await?;

    ctx.write_all(rows, list_args.include_flows)
}

pub async fn fetch_live_specs(
    ctx: &mut crate::CliContext,
    mut list: List,
) -> anyhow::Result<Vec<list_live_specs_query::SelectRef>> {
    use futures::TryStreamExt;

    if list.name_selector.name.is_empty() && list.name_selector.prefix.is_empty() {
        const DEFAULT_PREFIX_LIMIT: usize = 5;

        let role_list = auth::list::list_authorized_prefixes(
            ctx,
            models::Capability::Read,
            DEFAULT_PREFIX_LIMIT * 2,
        )
        .await?;

        let prefixes = filter_default_prefixes(role_list, DEFAULT_PREFIX_LIMIT)?;
        if prefixes.is_empty() {
            anyhow::bail!(
                "the current user does not have access to any catalog prefixes, please ask your tenant administrator for help"
            );
        }
        tracing::debug!(
            ?prefixes,
            "no --prefix argument provided, determined prefix automatically"
        );
        list.name_selector.prefix = prefixes;
    }

    fetch_paginated_live_specs(ctx.client.clone(), list)
        .try_collect()
        .await
}

/// Accepts a listing of the users role grants, and returns a filtered list of
/// prefixes to use for selecting live specs. This removes any unnecessary or
/// redundant role grants from `role_list` so that we can avoid making
/// unnecessary requests and needing to deal with duplicate specs.
fn filter_default_prefixes(
    mut role_list: Vec<auth::list::AuthorizedPrefix>,
    max: usize,
) -> anyhow::Result<Vec<String>> {
    // Filter out `ops/dp/` prefixes because there are never any live specs under that prefix.
    role_list.retain(|r| !r.prefix.starts_with("ops/dp/"));

    // Sort the remaining roles so that we can remove redundant prefixes. Top-level
    // prefixes will sort first, so we can ignore, e.g. `tenant/nested/` if there's
    // also a `tenant/` grant.
    role_list.sort_by(|l, r| l.prefix.cmp(&r.prefix));

    let mut prefixes: Vec<String> = Vec::new();
    for candidate in role_list {
        if prefixes
            .last()
            .is_some_and(|last| candidate.prefix.starts_with(last.as_str()))
        {
            continue;
        }
        prefixes.push(candidate.prefix.to_string());
    }

    if prefixes.len() > max {
        anyhow::bail!(
            "an explicit --prefix or --name argument is required since you have access to more than {max} prefixes.\nRun `flowctl auth roles list` to see the prefixes you can access"
        );
    }
    Ok(prefixes)
}

pub fn into_draft(
    specs: Vec<list_live_specs_query::SelectRef>,
) -> anyhow::Result<tables::DraftCatalog> {
    let mut catalog = tables::DraftCatalog::default();

    fn parse<T: serde::de::DeserializeOwned>(
        model: Option<&models::RawValue>,
    ) -> anyhow::Result<Option<T>> {
        if let Some(model) = model {
            Ok(Some(serde_json::from_str::<T>(model.get())?))
        } else {
            Ok(None)
        }
    }

    for row in specs {
        let list_live_specs_query::SelectRef {
            catalog_name,
            live_spec: Some(live_spec),
            ..
        } = row
        else {
            continue;
        };

        let scope = tables::synthetic_scope("control", &catalog_name);

        match live_spec.catalog_type {
            CatalogType::Capture => {
                catalog.captures.insert_row(
                    models::Capture::new(catalog_name),
                    &scope,
                    Some(live_spec.last_pub_id),
                    parse::<models::CaptureDef>(live_spec.model.as_ref())?,
                    false, // !is_touch
                );
            }
            CatalogType::Collection => {
                catalog.collections.insert_row(
                    models::Collection::new(catalog_name),
                    &scope,
                    Some(live_spec.last_pub_id),
                    parse::<models::CollectionDef>(live_spec.model.as_ref())?,
                    false, // !is_touch
                );
            }
            CatalogType::Materialization => {
                catalog.materializations.insert_row(
                    models::Materialization::new(catalog_name),
                    &scope,
                    Some(live_spec.last_pub_id),
                    parse::<models::MaterializationDef>(live_spec.model.as_ref())?,
                    false, // !is_touch
                );
            }
            CatalogType::Test => {
                catalog.tests.insert_row(
                    models::Test::new(catalog_name),
                    &scope,
                    Some(live_spec.last_pub_id),
                    parse::<models::TestDef>(live_spec.model.as_ref())?,
                    false, // !is_touch
                );
            }
        }
    }
    Ok(catalog)
}

impl output::CliOutput for list_live_specs_query::SelectRef {
    type TableAlt = bool;
    type CellValue = String;

    fn table_headers(flows: Self::TableAlt) -> Vec<&'static str> {
        let mut headers = vec![
            "ID",
            "Name",
            "Type",
            "Updated",
            "Updated By",
            "Data Plane ID",
        ];
        if flows {
            headers.push("Reads From");
            headers.push("Writes To");
        }
        headers
    }

    fn into_table_row(self, flows: Self::TableAlt) -> Vec<Self::CellValue> {
        let user_info = self
            .last_publication
            .map(|last_pub| {
                crate::format_user(
                    last_pub.user_email,
                    last_pub.user_full_name,
                    Some(last_pub.user_id),
                )
            })
            .unwrap_or_else(|| String::from("unknown"));
        let mut out = vec![
            self.live_spec
                .as_ref()
                .map(|ls| ls.live_spec_id.to_string())
                .unwrap_or_default(),
            self.catalog_name.to_string(),
            self.live_spec
                .as_ref()
                .map(|ls| ls.catalog_type.as_ref().to_string())
                .unwrap_or_default(),
            self.live_spec
                .as_ref()
                .map(|ls| ls.updated_at.to_rfc3339())
                .unwrap_or_default(),
            user_info,
            self.live_spec
                .as_ref()
                .map(|ls| ls.data_plane_id.to_string())
                .unwrap_or_default(),
        ];
        if flows {
            out.push(
                self.live_spec
                    .as_ref()
                    .map(|ls| format_flows(ls.reads_from.as_ref()))
                    .unwrap_or_default(),
            );
            out.push(
                self.live_spec
                    .as_ref()
                    .map(|ls| format_flows(ls.writes_to.as_ref()))
                    .unwrap_or_default(),
            );
        }
        out
    }
}

fn format_flows(conn: Option<&list_live_specs_query::SelectConnection>) -> String {
    use itertools::Itertools;

    conn.into_iter()
        .flat_map(|n| n.edges.iter())
        .map(|e| e.node.catalog_name.as_str())
        .join("\n")
}

/// Executes the graphql query for the given `list` arguments, making additional
/// requests as necessary to read all of the results.
fn fetch_paginated_live_specs(
    client: flow_client::Client,
    list: List,
) -> impl futures::Stream<Item = anyhow::Result<list_live_specs_query::SelectRef>> + 'static {
    if list.name_selector.name.is_empty() && list.name_selector.prefix.is_empty() {
        panic!("fetch_paginated_live_specs requires either a name or prefix selector");
    }
    // Use a smaller batch size if we're including the models, since they can be quite large.
    let page_size = if list.include_models { 50 } else { 200 };
    let is_by_name = !list.name_selector.name.is_empty();
    coroutines::try_coroutine(|mut co| async move {
        for query_by in to_vars(&list) {
            let mut cursor: Option<String> = None;

            'pagination: loop {
                let vars = list_live_specs_query::Variables {
                    by: query_by.clone(),
                    after: cursor.take(),
                    first: Some(page_size),
                    include_models: list.include_models,
                    include_flows: list.include_flows,
                    include_last_publication: list.include_last_publication,
                };
                let resp = post_graphql::<ListLiveSpecsQuery>(&client, vars)
                    .await
                    .context("failed to fetch live specs")?;

                for edge in resp.live_specs.edges {
                    // Only error when the user explicitly requested the spec by
                    // name and it does not exist. Otherwise, a missing live spec
                    // just indicates that the spec is in the process of being
                    // deleted.
                    if edge.node.live_spec.is_none() && is_by_name {
                        anyhow::bail!("no live spec exists for name: '{}'", edge.node.catalog_name);
                    }
                    let () = co.yield_(edge.node).await;
                }
                if !resp.live_specs.page_info.has_next_page {
                    break 'pagination;
                }
                cursor = resp.live_specs.page_info.end_cursor;
                assert!(cursor.is_some(), "liveSpecs pageInfo missing endCursor");
            }
        }
        Ok(())
    })
}

fn to_vars(list: &List) -> Vec<list_live_specs_query::LiveSpecsBy> {
    let data_plane_name = list
        .data_plane_selector
        .data_plane_name
        .as_deref()
        .map(models::Name::new);
    let catalog_type = list.type_selector.get_single_type_selection();

    let mut vars = Vec::new();
    for prefix in list.name_selector.prefix.iter() {
        let by = list_live_specs_query::LiveSpecsBy::PrefixAndType(
            list_live_specs_query::ByPrefixAndType {
                prefix: models::Prefix::new(prefix),
                catalog_type,
                data_plane_name: data_plane_name.clone(),
            },
        );
        vars.push(by);
    }
    if !list.name_selector.name.is_empty() {
        let names = list
            .name_selector
            .name
            .iter()
            .map(|n| models::Name::new(n.as_str()))
            .collect();
        vars.push(list_live_specs_query::LiveSpecsBy::Names(names));
    }
    vars
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::auth::list::AuthorizedPrefix;

    #[test]
    fn test_filter_default_prefixes() {
        fn pre(prefix: &str) -> AuthorizedPrefix {
            AuthorizedPrefix {
                prefix: models::Prefix::new(prefix),
                user_capability: models::Capability::Admin, // irrelevant
            }
        }
        let roles = vec![
            pre("wileyCo/"),
            pre("acmeCo/prod/anvils/"),
            pre("acmeCo/dev/anvils/"),
            pre("acmeCo/dev/tnt/"),
            pre("acmeCo/"),
            pre("acmeCo/prod/"),
            pre("acmeCo/foo/"),
            pre("coyoteCo/"),
        ];
        let result = filter_default_prefixes(roles.clone(), 3).expect("should return 3 prefixes");
        assert_eq!(
            vec![
                "acmeCo/".to_string(),
                "coyoteCo/".to_string(),
                "wileyCo/".to_string(),
            ],
            result
        );

        let fail_result = filter_default_prefixes(roles, 2);
        assert!(fail_result.is_err());
    }
}
