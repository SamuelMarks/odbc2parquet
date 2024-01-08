use anyhow::{bail, Context, Error};
use log::{debug, info};
use odbc_api::{buffers::ColumnarAnyBuffer, BlockCursor, Cursor, ColumnDescription, ResultSetMetadata};
use parquet::schema::types::{Type, TypePtr};
use std::sync::Arc;

use crate::{
    batch_size_limit::BatchSizeLimit,
    column_strategy::{strategy_from_column_description, ColumnStrategy, MappingOptions},
    parquet_buffer::ParquetBuffer, parquet_writer::ParquetOutput
};

/// Contains the decisions of how to fetch each columns of a table from an ODBC data source and copy
/// it into a parquet file. This decisions include what kind of ODBC C_TYPE to use to fetch the data
/// and in what these columns are transformed.
pub struct TableStrategy {
    columns: Vec<ColumnInfo>,
    parquet_schema: TypePtr,
}

/// Name, ColumnStrategy
type ColumnInfo = (String, Box<dyn ColumnStrategy>);

impl TableStrategy {
    pub fn new(
        cursor: &mut impl ResultSetMetadata,
        mapping_options: MappingOptions,
    ) -> Result<Self, Error> {
        let num_cols = cursor.num_result_cols()?;

        let mut columns = Vec::new();

        for index in 1..(num_cols + 1) {
            let mut cd = ColumnDescription::default();
            // Reserving helps with drivers not reporting column name size correctly.
            cd.name.reserve(128);
            cursor.describe_col(index as u16, &mut cd)?;

            debug!("ODBC column description for column {}: {:?}", index, cd);

            let name = cd.name_to_string()?;
            // Give a generated name, should we fail to retrieve one from the ODBC data source.
            let name = if name.is_empty() {
                format!("Column{index}")
            } else {
                name
            };

            let column_fetch_strategy =
                strategy_from_column_description(&cd, &name, mapping_options, cursor, index)?;
            columns.push((name, column_fetch_strategy));
        }

        if columns.is_empty() {
            bail!("Resulting parquet file would not have any columns!")
        }

        let fields = columns
            .iter()
            .map(|(name, s)| Arc::new(s.parquet_type(name)))
            .collect();
        let parquet_schema = Arc::new(
            Type::group_type_builder("schema")
                .with_fields(fields)
                .build()
                .unwrap(),
        );

        Ok(TableStrategy { columns, parquet_schema })
    }

    pub fn allocate_fetch_buffer(
        &self,
        batch_size: BatchSizeLimit,
    ) -> Result<ColumnarAnyBuffer, Error> {
        let mem_usage_odbc_buffer_per_row: usize = self
            .columns
            .iter()
            .map(|(_name, strategy)| strategy.buffer_desc().bytes_per_row())
            .sum();
        let total_mem_usage_per_row =
            mem_usage_odbc_buffer_per_row + ParquetBuffer::MEMORY_USAGE_BYTES_PER_ROW;
        info!(
            "Memory usage per row is {} bytes. This excludes memory directly allocated by the ODBC \
            driver.",
            total_mem_usage_per_row,
        );

        let batch_size_row = batch_size.batch_size_in_rows(total_mem_usage_per_row)?;

        info!("Batch size set to {} rows.", batch_size_row);

        let fetch_buffer = ColumnarAnyBuffer::from_descs(
            batch_size_row,
            self.columns
                .iter()
                .map(|(_name, strategy)| (strategy.buffer_desc())),
        );

        Ok(fetch_buffer)
    }

    pub fn parquet_schema(&self) -> TypePtr {
        self.parquet_schema.clone()
    }

    pub fn block_cursor_to_parquet(
        &self,
        mut row_set_cursor: BlockCursor<impl Cursor, &mut ColumnarAnyBuffer>,
        mut writer: Box<dyn ParquetOutput>,
    ) -> Result<(), Error> {
        let mut num_batch = 0;
        // Count the number of total rows fetched so far for logging. This should be identical to
        // `num_batch * batch_size_row + num_rows`.
        let mut total_rows_fetched = 0;
    
        let mut pb = ParquetBuffer::new(row_set_cursor.row_array_size());
    
        while let Some(buffer) = row_set_cursor
            .fetch()
            .map_err(give_hint_about_flag_for_oracle_users)?
        {
            let mut row_group_writer = writer.next_row_group(num_batch)?;
            let mut col_index = 0;
            let num_rows = buffer.num_rows();
            total_rows_fetched += num_rows;
            num_batch += 1;
            info!("Fetched batch {num_batch} with {num_rows} rows.");
            info!("Fetched {total_rows_fetched} rows in total.");
            pb.set_num_rows_fetched(num_rows);
            while let Some(mut column_writer) = row_group_writer.next_column()? {
                let col_name = self.parquet_schema.get_fields()[col_index]
                    .get_basic_info()
                    .name();
                debug!(
                    "Writing column with index {} and name '{}'.",
                    col_index, col_name
                );
    
                let odbc_column = buffer.column(col_index);
    
                self.columns[col_index]
                    .1
                    .copy_odbc_to_parquet(&mut pb, column_writer.untyped(), odbc_column)
                    .with_context(|| {
                        format!(
                            "Failed to copy column '{col_name}' from ODBC representation into \
                            Parquet."
                        )
                    })?;
                column_writer.close()?;
                col_index += 1;
            }
            let metadata = row_group_writer.close()?;
            writer.update_current_file_size(metadata.compressed_size());
        }
        writer.close_box()?;
        Ok(())
    }
}

/// If we hit the issue with oracle not supporting 64Bit, let's tell our users that we have
/// implemented a solution to it.
fn give_hint_about_flag_for_oracle_users(error: odbc_api::Error) -> Error {
    match error {
        error @ odbc_api::Error::OracleOdbcDriverDoesNotSupport64Bit(_) => {
            let error: Error = error.into();
            error.context(
                "Looks like you are using an Oracle database. Try the \
                `--driver-does-not-support-64bit-integers` flag.",
            )
        }
        other => other.into(),
    }
}