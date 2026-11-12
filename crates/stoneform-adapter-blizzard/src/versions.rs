use stoneform_app::ports::{CdnHosts, LiveBuild, RemoteError, VersionSource};
use stoneform_domain::Region;
use stoneform_tact::bpsv::Table;
use stoneform_tact::config::{select_cdn, select_version};
use stoneform_tact::keys::ContentKey;

use crate::{BlizzardHttp, VersionTable};

/// Where product configs live when the `cdns` row doesn't say.
const DEFAULT_CONFIG_PATH: &str = "tpr/configs/data";

/// The `VersionSource` port on Blizzard's HTTPS version service.
#[derive(Debug, Clone)]
pub struct HttpVersionSource {
    http: BlizzardHttp,
}

impl HttpVersionSource {
    pub fn new(http: BlizzardHttp) -> Self {
        Self { http }
    }
}

impl VersionSource for HttpVersionSource {
    async fn live(&self, product: &str, region: Region) -> Result<LiveBuild, RemoteError> {
        let region = region.as_str();
        let (versions, cdns) = tokio::try_join!(
            self.http.version_table(region, product, VersionTable::Versions),
            self.http.version_table(region, product, VersionTable::Cdns),
        )
        .map_err(RemoteError::new)?;
        let version = select_version(&Table::parse(&versions), region).map_err(RemoteError::new)?;
        let cdn = select_cdn(&Table::parse(&cdns), region).map_err(RemoteError::new)?;
        let key = |hex: &str| ContentKey::from_hex(hex).map_err(RemoteError::new);
        Ok(LiveBuild {
            build_config: key(&version.build_config)?,
            cdn_config: key(&version.cdn_config)?,
            product_config: version.product_config.as_deref().map(key).transpose()?,
            version: version.name,
            key_ring: version.key_ring,
            cdn: CdnHosts {
                path: cdn.path,
                config_path: cdn.config_path.unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_owned()),
                servers: cdn.servers,
                hosts_field: cdn.hosts.join(" "),
                servers_field: cdn.servers_field,
            },
        })
    }
}
