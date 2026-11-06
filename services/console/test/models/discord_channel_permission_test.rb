require "test_helper"

class DiscordChannelPermissionTest < ActiveSupport::TestCase
  test "has an opaque id" do
    permission = DiscordChannelPermission.create!(
      principal: principals(:acme_channel),
      channel_id: "123456789012345678",
      upload_enabled: true
    )

    assert_match(/\Adcp_[A-Za-z0-9]+\z/, permission.oid)
    assert_equal permission, DiscordChannelPermission.find_by_oid!(permission.oid)
  end

  test "normalizes channel id and requires a snowflake and at least one permission" do
    permission = DiscordChannelPermission.new(
      principal: principals(:acme_channel),
      channel_id: " 123456789012345678 ",
      upload_enabled: true,
      download_enabled: false,
      history_enabled: false
    )

    assert_predicate permission, :valid?
    permission.save!
    assert_equal "123456789012345678", permission.channel_id

    empty = DiscordChannelPermission.new(
      principal: principals(:acme_channel),
      channel_id: "987654321098765432"
    )
    assert_not empty.valid?
    assert_includes empty.errors[:base], "Select at least one Discord permission"

    %w[C0123456789 general 1234567890123456 123456789012345678901].each do |channel_id|
      invalid = DiscordChannelPermission.new(
        principal: principals(:acme_channel),
        channel_id: channel_id,
        upload_enabled: true
      )
      assert_not invalid.valid?, channel_id
      assert_includes invalid.errors[:channel_id], "is not a valid Discord channel ID"
    end
  end

  test "channel id cannot change after creation" do
    permission = DiscordChannelPermission.create!(
      principal: principals(:acme_channel),
      channel_id: "123456789012345678",
      upload_enabled: true
    )

    assert_raises(ActiveRecord::ReadonlyAttributeError) do
      permission.update!(channel_id: "987654321098765432")
    end
    assert_equal "123456789012345678", permission.reload.channel_id
  end

  test "replace_for replaces principal permission rows" do
    principal = principals(:acme_channel)

    DiscordChannelPermission.replace_for!(
      principal,
      [
        {
          channel_id: " 123456789012345678 ",
          upload_enabled: true,
          download_enabled: true,
          history_enabled: false
        }
      ]
    )

    permission = principal.discord_channel_permissions.reload.sole
    assert_equal "123456789012345678", permission.channel_id
    assert_equal true, permission.upload_enabled
    assert_equal true, permission.download_enabled
    assert_equal false, permission.history_enabled
  end

  test "role permissions require exactly one grantee and are unique per channel" do
    role = roles(:acme_infra)
    permission = DiscordChannelPermission.new(
      role: role,
      channel_id: "123456789012345678",
      upload_enabled: true
    )

    assert_predicate permission, :valid?
    permission.save!

    duplicate = role.discord_channel_permissions.new(
      channel_id: "123456789012345678",
      history_enabled: true
    )
    assert_not duplicate.valid?
    assert_includes duplicate.errors[:channel_id], "has already been taken"

    missing = DiscordChannelPermission.new(channel_id: "987654321098765432", upload_enabled: true)
    assert_not missing.valid?
    assert_includes missing.errors[:base], "must reference exactly one of principal, role"

    both = DiscordChannelPermission.new(
      principal: principals(:acme_channel),
      role: role,
      channel_id: "987654321098765432",
      upload_enabled: true
    )
    assert_not both.valid?
    assert_includes both.errors[:base], "must reference exactly one of principal, role"
  end

  test "database requires exactly one grantee" do
    assert_raises ActiveRecord::StatementInvalid do
      DiscordChannelPermission.transaction(requires_new: true) do
        DiscordChannelPermission.insert_all!([
          {
            principal_id: principals(:acme_channel).id,
            role_id: roles(:acme_infra).id,
            channel_id: "123456789012345678",
            upload_enabled: true,
            download_enabled: false,
            history_enabled: false,
            created_at: Time.current,
            updated_at: Time.current
          }
        ])
      end
    end
  end

  test "replace_for merges duplicate role rows and bumps assigned principals" do
    role = roles(:acme_infra)
    versions = Principal.where(id: role.principal_ids).pluck(:id, :sync_config_cache_version).to_h

    DiscordChannelPermission.replace_for!(
      role,
      [
        {
          channel_id: " 123456789012345678 ",
          upload_enabled: "1",
          download_enabled: "0",
          history_enabled: "0"
        },
        {
          channel_id: "123456789012345678",
          upload_enabled: "0",
          download_enabled: "1",
          history_enabled: "1"
        }
      ]
    )

    permission = role.discord_channel_permissions.reload.sole
    assert_equal "123456789012345678", permission.channel_id
    assert_predicate permission, :upload_enabled
    assert_predicate permission, :download_enabled
    assert_predicate permission, :history_enabled
    Principal.where(id: role.principal_ids).find_each do |principal|
      assert_equal versions.fetch(principal.id) + 1, principal.sync_config_cache_version
    end
  end

  test "invalid role replacement rolls back rows and cache versions" do
    role = roles(:acme_infra)
    permission = role.discord_channel_permissions.create!(
      channel_id: "123456789012345678",
      upload_enabled: true
    )
    versions = Principal.where(id: role.principal_ids).pluck(:id, :sync_config_cache_version).to_h

    assert_raises ActiveRecord::RecordInvalid do
      DiscordChannelPermission.replace_for!(
        role,
        [
          {
            channel_id: "987654321098765432",
            upload_enabled: false,
            download_enabled: false,
            history_enabled: false
          }
        ]
      )
    end

    assert_equal [ permission.id ], role.discord_channel_permissions.reload.pluck(:id)
    Principal.where(id: role.principal_ids).find_each do |principal|
      assert_equal versions.fetch(principal.id), principal.sync_config_cache_version
    end
  end

  test "role permission changes invalidate every assigned principal" do
    role = roles(:acme_infra)
    principal_ids = role.principal_ids
    versions = Principal.where(id: principal_ids).pluck(:id, :sync_config_cache_version).to_h

    permission = role.discord_channel_permissions.create!(
      channel_id: "123456789012345678",
      upload_enabled: true
    )
    Principal.where(id: principal_ids).find_each do |principal|
      assert_equal versions.fetch(principal.id) + 1, principal.sync_config_cache_version
    end

    versions = Principal.where(id: principal_ids).pluck(:id, :sync_config_cache_version).to_h
    permission.update!(history_enabled: true)
    Principal.where(id: principal_ids).find_each do |principal|
      assert_equal versions.fetch(principal.id) + 1, principal.sync_config_cache_version
    end

    versions = Principal.where(id: principal_ids).pluck(:id, :sync_config_cache_version).to_h
    permission.destroy!
    Principal.where(id: principal_ids).find_each do |principal|
      assert_equal versions.fetch(principal.id) + 1, principal.sync_config_cache_version
    end
  end
end
