class CreateDiscordChannelPermissions < ActiveRecord::Migration[8.1]
  def change
    create_table :discord_channel_permissions do |t|
      t.references :principal, null: true, foreign_key: true, index: false
      t.references :role, null: true, foreign_key: true, index: false
      t.string :channel_id, null: false
      t.boolean :upload_enabled, null: false, default: false
      t.boolean :download_enabled, null: false, default: false
      t.boolean :history_enabled, null: false, default: false

      t.timestamps
    end

    add_index :discord_channel_permissions,
              %i[principal_id channel_id],
              unique: true,
              where: "principal_id IS NOT NULL",
              name: "idx_discord_permissions_unique_principal_channel"
    add_index :discord_channel_permissions,
              %i[role_id channel_id],
              unique: true,
              where: "role_id IS NOT NULL",
              name: "idx_discord_permissions_unique_role_channel"
    add_check_constraint :discord_channel_permissions,
                         "(principal_id IS NOT NULL) <> (role_id IS NOT NULL)",
                         name: "discord_channel_permissions_exactly_one_grantee"
  end
end
