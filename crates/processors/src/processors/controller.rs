use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use async_trait::async_trait;
use dojo_world::contracts::world::WorldContractReader;
use lazy_static::lazy_static;
use starknet::core::types::Event;
use starknet::core::utils::parse_cairo_short_string;
use starknet::macros::felt;
use starknet::providers::Provider;
use starknet_crypto::Felt;
use torii_sqlite::error::ParseError;
use torii_sqlite::Sql;
use tracing::info;

use crate::error::Error;
use crate::task_manager::TaskId;
use crate::{EventProcessor, EventProcessorConfig};

pub(crate) const LOG_TARGET: &str = "torii::indexer::processors::controller";

#[derive(Default, Debug)]
pub struct ControllerProcessor;

lazy_static! {
    // https://x.cartridge.gg/
    pub(crate) static ref CARTRIDGE_MAGIC: [Felt; 22] = [
        felt!("0x68"),
        felt!("0x74"),
        felt!("0x74"),
        felt!("0x70"),
        felt!("0x73"),
        felt!("0x3a"),
        felt!("0x2f"),
        felt!("0x2f"),
        felt!("0x78"),
        felt!("0x2e"),
        felt!("0x63"),
        felt!("0x61"),
        felt!("0x72"),
        felt!("0x74"),
        felt!("0x72"),
        felt!("0x69"),
        felt!("0x64"),
        felt!("0x67"),
        felt!("0x65"),
        felt!("0x2e"),
        felt!("0x67"),
        felt!("0x67"),
    ];
}

#[async_trait]
impl<P> EventProcessor<P> for ControllerProcessor
where
    P: Provider + Send + Sync + std::fmt::Debug + 'static,
{
    fn event_key(&self) -> String {
        "ContractDeployed".to_string()
    }

    fn validate(&self, event: &Event) -> bool {
        // ContractDeployed event has no keys and contains username in data
        event.keys.len() == 1 && !event.data.is_empty()
    }

    fn task_identifier(&self, event: &Event) -> TaskId {
        let mut hasher = DefaultHasher::new();
        // the contract address is the first felt in data
        event.data[0].hash(&mut hasher);
        hasher.finish()
    }

    async fn process(
        &self,
        _world: Arc<WorldContractReader<P>>,
        db: &mut Sql,
        _block_number: u64,
        block_timestamp: u64,
        _event_id: &str,
        event: &Event,
        _config: &EventProcessorConfig,
    ) -> Result<(), Error> {
        // Address is the first felt in data
        let address = event.data[0];

        let calldata = event.data[5..].to_vec();
        // our calldata has to be more than 25 felts.
        if calldata.len() < 25 {
            return Ok(());
        }
        // check for this sequence of felts
        let cartridge_magic_len = calldata[2];
        // length has to be 22
        if cartridge_magic_len != Felt::from(22) {
            return Ok(());
        }

        // this should never fail if since our len is 22
        let cartridge_magic: [Felt; 22] = calldata[3..25].try_into().unwrap();

        // has to match with https://x.cartridge.gg/
        if !CARTRIDGE_MAGIC.eq(&cartridge_magic) {
            return Ok(());
        }

        // Last felt in data is the salt which is the username encoded as short string
        let username_felt = event.data[event.data.len() - 1];
        let username = parse_cairo_short_string(&username_felt)
            .map_err(|e| Error::ParseError(ParseError::ParseCairoShortString(e)))?;

        info!(
            target: LOG_TARGET,
            username = %username,
            address = %format!("{address:#x}"),
            "Controller deployed."
        );

        db.add_controller(&username, &format!("{address:#x}"), block_timestamp)
            .await?;

        Ok(())
    }
}
