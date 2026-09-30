//! The station's beliefs (link, custodian and channel models), kept as
//! opaque records between runs.

use redb::ReadableTable;

use crate::{Result, Store, BELIEFS};

impl Store {
    /// Save belief records in one transaction: `(key, Some(value))` writes,
    /// `(key, None)` deletes.
    pub fn save_beliefs(&self, records: &[(Vec<u8>, Option<Vec<u8>>)]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let tx = self.write_tx()?;
        {
            let mut table = tx.open_table(BELIEFS)?;
            for (key, value) in records {
                match value {
                    Some(value) => {
                        table.insert(key.as_slice(), value.as_slice())?;
                    }
                    None => {
                        table.remove(key.as_slice())?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Every saved belief record.
    pub fn beliefs(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let tx = self.read_tx()?;
        let table = tx.open_table(BELIEFS)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            out.push((key.value().to_vec(), value.value().to_vec()));
        }
        Ok(out)
    }
}
