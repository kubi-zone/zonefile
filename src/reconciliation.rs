use futures::StreamExt;
use kubizone_crds::{
    kubizone_common::FullyQualifiedDomainName,
    v1alpha1::{Zone, ZoneEntry},
};
use zonefile_crds::{ZoneFile, TARGET_ZONEFILE_LABEL};

use k8s_openapi::{api::core::v1::ConfigMap, serde_json::json};
use kube::{
    api::{Patch, PatchParams},
    core::ObjectMeta,
    runtime::{controller::Action, watcher, Controller},
    Api, Client, Resource as _, ResourceExt as _,
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tracing::log::*;

struct Data {
    client: Client,
}

pub const CONTROLLER_NAME: &str = "kubi.zone/zonefile";

fn build_zonefile(origin: &FullyQualifiedDomainName, entries: &[ZoneEntry]) -> String {
    // We use the longest domain name in the list for
    // aligning the text in the output zonefile
    let longest_name_length = entries
        .iter()
        .map(|entry| entry.fqdn.to_string().len())
        .max()
        .unwrap_or_default();

    let serialized_records = entries
        .iter()
        .map(
            |ZoneEntry {
                 fqdn,
                 type_,
                 class,
                 ttl,
                 rdata,
                 ..
             }| {
                let name = match fqdn.clone() - origin.clone() {
                    Ok(partial) => partial.to_string(),
                    Err(full) => full.to_string(),
                };

                let entry = if name.is_empty() { "@" } else { &name };

                format!(
                    "{entry:<width$} {ttl:<8} {class:<5} {type_:<6} {rdata}",
                    width = longest_name_length
                )
            },
        )
        .collect::<Vec<_>>()
        .join("\n");

    format!("$ORIGIN {origin}\n\n{serialized_records}")
}

/// Applied a [`TARGET_ZONEFILE_LABEL`] label which references our zonefile.
/// This label is monitored by our controller, causing reconciliation loops
/// to fire for [`ZoneFile`]s referenced by [`Zone`]s, when the zone itself
/// is updated.
async fn apply_zonefile_backref(
    client: Client,
    zonefile: &ZoneFile,
    zone: &Zone,
) -> Result<(), kube::Error> {
    let zonefile_ref = format!(
        "{}.{}",
        zonefile.name_any(),
        zonefile.namespace().as_ref().unwrap()
    );

    if zone.labels().get(TARGET_ZONEFILE_LABEL) != Some(&zonefile_ref) {
        info!(
            "updating zone {}'s {TARGET_ZONEFILE_LABEL} to {zonefile_ref}",
            zonefile.name_any()
        );

        Api::<Zone>::namespaced(client, zone.namespace().as_ref().unwrap())
            .patch_metadata(
                &zone.name_any(),
                &PatchParams::apply(CONTROLLER_NAME),
                &Patch::Merge(json!({
                    "metadata": {
                        "labels": {
                            TARGET_ZONEFILE_LABEL: zonefile_ref
                        },
                    }
                })),
            )
            .await?;
    }

    Ok(())
}

async fn reconcile_zonefiles(
    zonefile: Arc<ZoneFile>,
    ctx: Arc<Data>,
) -> Result<Action, kube::Error> {
    struct SerializedZone {
        origin: String,
        serial: u32,
        hash: String,
        contents: String,
    }

    let mut serialized_zones = Vec::new();

    for zone_ref in &zonefile.spec.zone_refs {
        let zone = Api::<Zone>::namespaced(
            ctx.client.clone(),
            &zone_ref
                .namespace
                .as_ref()
                .or(zonefile.namespace().as_ref())
                .cloned()
                .unwrap(),
        )
        .get(&zone_ref.name)
        .await?;

        apply_zonefile_backref(ctx.client.clone(), &zonefile, &zone).await?;

        let Some(origin) = zone.fqdn() else {
            debug!("zone {zone} has no fqdn, skipping.");
            continue;
        };

        let Some(hash) = zone.hash() else {
            debug!("zone {zone} has not computed its hash yet, skipping");
            continue;
        };

        let Some(serial) = zone.serial() else {
            debug!("zone {zone} has not produced a serial yet, skipping");
            continue;
        };

        let serialized_zone = build_zonefile(origin, &zone.status.as_ref().unwrap().entries);

        serialized_zones.push(SerializedZone {
            origin: origin.to_string(),
            serial,
            hash: hash.to_string(),
            contents: serialized_zone,
        });
    }

    let owner_reference = zonefile.controller_owner_ref(&()).unwrap();
    let configmap_name = zonefile
        .spec
        .config_map_name
        .as_ref()
        .cloned()
        .unwrap_or(zonefile.name_any());

    let config_map = ConfigMap {
        metadata: ObjectMeta {
            name: Some(configmap_name.clone()),
            namespace: zonefile.namespace(),
            owner_references: Some(vec![owner_reference]),
            ..ObjectMeta::default()
        },
        data: Some(BTreeMap::from_iter(serialized_zones.iter().map(
            |serialized_zone| {
                (
                    serialized_zone.origin.clone(),
                    serialized_zone.contents.clone(),
                )
            },
        ))),
        ..Default::default()
    };

    Api::<ConfigMap>::namespaced(ctx.client.clone(), zonefile.namespace().as_ref().unwrap())
        .patch(
            &configmap_name,
            &PatchParams::apply(CONTROLLER_NAME),
            &Patch::Apply(config_map),
        )
        .await?;

    Api::<ZoneFile>::namespaced(ctx.client.clone(), zonefile.namespace().as_ref().unwrap())
        .patch_status(
            &zonefile.name_any(),
            &PatchParams::apply(CONTROLLER_NAME),
            &Patch::Merge(json!({
                "status": {
                    "hash": BTreeMap::from_iter(serialized_zones.iter().map(|serialized_zone| (&serialized_zone.origin, &serialized_zone.hash))),
                    "serial": BTreeMap::from_iter(serialized_zones.iter().map(|serialized_zone| (&serialized_zone.origin, serialized_zone.serial))),
                },
            })),
        )
        .await?;

    Ok(Action::requeue(Duration::from_secs(300)))
}

fn zonefile_error_policy(zone: Arc<ZoneFile>, error: &kube::Error, _ctx: Arc<Data>) -> Action {
    error!(
        "zonefile {} reconciliation encountered error: {error}",
        zone.name_any()
    );
    Action::requeue(Duration::from_secs(60))
}

pub async fn reconcile(client: Client) {
    let zonefiles = Api::<ZoneFile>::all(client.clone());

    let zone_controller = Controller::new(zonefiles, watcher::Config::default())
        .watches(
            Api::<Zone>::all(client.clone()),
            watcher::Config::default(),
            kubizone_crds::watch_reference(TARGET_ZONEFILE_LABEL),
        )
        .shutdown_on_signal()
        .run(
            reconcile_zonefiles,
            zonefile_error_policy,
            Arc::new(Data {
                client: client.clone(),
            }),
        )
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled {:?}", o),
                Err(e) => warn!("reconcile failed: {}", e),
            }
        });

    zone_controller.await;
}

#[cfg(test)]
mod tests {
    use kubizone_common::{Class, FullyQualifiedDomainName, Type};
    use kubizone_crds::v1alpha1::ZoneEntry;

    use super::build_zonefile;

    #[test]
    fn zonefile_construction() {
        let origin = FullyQualifiedDomainName::try_from("example.org.").unwrap();

        let entries = vec![
            ZoneEntry {
                fqdn: FullyQualifiedDomainName::try_from("www.example.org.").unwrap(),
                type_: Type::A,
                class: Class::IN,
                ttl: 360,
                rdata: "127.0.0.1".to_string(),
            },
            ZoneEntry {
                fqdn: FullyQualifiedDomainName::try_from("example.org.").unwrap(),
                type_: Type::CNAME,
                class: Class::IN,
                ttl: 360,
                rdata: "www.example.org.".to_string(),
            },
        ];

        let zonefile = build_zonefile(&origin, &entries);

        assert_eq!(
            zonefile,
            indoc::indoc! { r#"
            $ORIGIN example.org.

            www              360      IN A 127.0.0.1
            @                360      IN CNAME www.example.org."#
            }
        );
    }
}
