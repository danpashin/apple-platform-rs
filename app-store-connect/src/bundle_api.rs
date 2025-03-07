// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::{profile_api::ProfilesResponse, AppStoreConnectClient, Result};
use serde::{Deserialize, Serialize};

const APPLE_BUNDLE_IDS_URL: &str = "https://api.appstoreconnect.apple.com/v1/bundleIds";
const APPLE_BUNDLE_CAPABILITIES_URL: &str =
    "https://api.appstoreconnect.apple.com/v1/bundleIdCapabilities";

impl AppStoreConnectClient {
    pub fn register_bundle_id(&self, identifier: &str, name: &str) -> Result<BundleIdResponse> {
        let token = self.get_token()?;
        let body = BundleIdCreateRequest {
            data: BundleIdCreateRequestData {
                attributes: BundleIdCreateRequestAttributes {
                    identifier: identifier.into(),
                    name: name.into(),
                    platform: "UNIVERSAL".into(),
                },
                r#type: "bundleIds".into(),
            },
        };
        let req = self
            .client
            .post(APPLE_BUNDLE_IDS_URL)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .json(&body);
        Ok(self.send_request(req)?.json()?)
    }

    pub fn list_bundle_ids(&self) -> Result<BundleIdsResponse> {
        let token = self.get_token()?;
        let req = self
            .client
            .get(APPLE_BUNDLE_IDS_URL)
            .bearer_auth(token)
            .header("Accept", "application/json");
        Ok(self.send_request(req)?.json()?)
    }

    pub fn get_bundle_id(&self, id: &str) -> Result<BundleIdResponse> {
        let token = self.get_token()?;
        let req = self
            .client
            .get(format!("{APPLE_BUNDLE_IDS_URL}/{id}"))
            .bearer_auth(token)
            .header("Accept", "application/json");
        Ok(self.send_request(req)?.json()?)
    }

    pub fn list_bundle_profiles(&self, id: &str) -> Result<ProfilesResponse> {
        let token = self.get_token()?;
        let req = self
            .client
            .get(format!("{APPLE_BUNDLE_IDS_URL}/{id}/profiles"))
            .bearer_auth(token)
            .header("Accept", "application/json");
        Ok(self.send_request(req)?.json()?)
    }

    pub fn list_bundle_capabilities(&self, id: &str) -> Result<BundleCapabilitiesResponse> {
        let token = self.get_token()?;
        let req = self
            .client
            .get(format!("{APPLE_BUNDLE_IDS_URL}/{id}/bundleIdCapabilities"))
            .bearer_auth(token)
            .header("Accept", "application/json");
        Ok(self.send_request(req)?.json()?)
    }

    pub fn enable_bundle_id_capability(
        &self,
        id: &str,
        capability: BundleIdCapabilityCreateRequestDataAttributes,
    ) -> Result<()> {
        let token = self.get_token()?;

        let body = BundleIdCapabilityCreateRequest {
            data: BundleIdCapabilityCreateRequestData {
                attributes: capability,
                relationships: BundleIdCapabilityCreateRequestDataRelationships {
                    bundle_id: BundleIdCapabilityCreateRequestDataRelationshipBundleId {
                        data: BundleIdCapabilityCreateRequestDataRelationshipBundleIdData {
                            id: id.to_string(),
                            r#type: "bundleIds".to_string(),
                        },
                    },
                },
                r#type: "bundleIdCapabilities".to_string(),
            },
        };

        let req = self
            .client
            .post(APPLE_BUNDLE_CAPABILITIES_URL)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .json(&body);
        self.send_request(req)?;
        Ok(())
    }

    pub fn delete_bundle_id(&self, id: &str) -> Result<()> {
        let token = self.get_token()?;
        let req = self
            .client
            .delete(format!("{APPLE_BUNDLE_IDS_URL}/{id}"))
            .bearer_auth(token);
        self.send_request(req)?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleIdCreateRequest {
    pub data: BundleIdCreateRequestData,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleIdCreateRequestData {
    pub attributes: BundleIdCreateRequestAttributes,
    pub r#type: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleIdCreateRequestAttributes {
    pub identifier: String,
    pub name: String,
    pub platform: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
pub enum BundleIdPlatform {
    Ios,
    MacOs,
}

impl std::fmt::Display for BundleIdPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            Self::Ios => "IOS",
            Self::MacOs => "MAC_OS",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleIdResponse {
    pub data: BundleId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleIdsResponse {
    pub data: Vec<BundleId>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleId {
    pub attributes: BundleIdAttributes,
    pub id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleIdAttributes {
    pub identifier: String,
    pub name: String,
    pub platform: String,
    pub seed_id: String,
}

#[derive(Debug, Deserialize)]
pub struct BundleCapabilitiesResponse {
    pub data: Vec<BundleCapability>,
}

#[derive(Debug, Deserialize)]
pub struct BundleCapability {
    pub attributes: BundleCapabilityAttributes,
    pub id: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleCapabilityAttributes {
    pub capability_type: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleIdCapabilityCreateRequest {
    data: BundleIdCapabilityCreateRequestData,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleIdCapabilityCreateRequestData {
    attributes: BundleIdCapabilityCreateRequestDataAttributes,
    relationships: BundleIdCapabilityCreateRequestDataRelationships,
    r#type: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleIdCapabilityCreateRequestDataAttributes {
    pub capability_type: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleIdCapabilityCreateRequestDataRelationships {
    bundle_id: BundleIdCapabilityCreateRequestDataRelationshipBundleId,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleIdCapabilityCreateRequestDataRelationshipBundleId {
    data: BundleIdCapabilityCreateRequestDataRelationshipBundleIdData,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleIdCapabilityCreateRequestDataRelationshipBundleIdData {
    id: String,
    r#type: String,
}
