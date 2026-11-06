class BackfillDiscordChannelPermissionsFromLabels < ActiveRecord::Migration[8.1]
  def up
    execute <<~SQL.squish
      INSERT INTO discord_channel_permissions (
        principal_id,
        channel_id,
        upload_enabled,
        download_enabled,
        history_enabled,
        created_at,
        updated_at
      )
      SELECT
        principals.id,
        trim(principals.labels->>'discord_channel_id'),
        TRUE,
        TRUE,
        TRUE,
        CURRENT_TIMESTAMP,
        CURRENT_TIMESTAMP
      FROM principals
      WHERE trim(principals.labels->>'discord_channel_id') ~ '^[0-9]{17,20}$'
      ON CONFLICT (principal_id, channel_id) WHERE principal_id IS NOT NULL DO NOTHING
    SQL
  end

  def down
    # One-way data backfill. Existing operators may edit these permissions after
    # migration, so rollback should not delete potentially modified rows.
  end
end
