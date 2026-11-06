require "test_helper"

module Console
  class DiscordChannelPermissionsControllerTest < ActionDispatch::IntegrationTest
    setup do
      @operator = users(:acme_admin)
      post login_url, params: { email: @operator.email, password: "password123456" }
    end

    test "destroy deletes a principal permission and redirects to the principal" do
      principal = principals(:acme_user_bob)
      delete_permission = principal.discord_channel_permissions.create!(
        channel_id: "123456789012345678",
        upload_enabled: true
      )
      keep_permission = principal.discord_channel_permissions.create!(
        channel_id: "987654321098765432",
        upload_enabled: true
      )

      assert_difference -> { principal.discord_channel_permissions.count }, -1 do
        delete console_discord_channel_permission_url(delete_permission.oid)
      end

      assert_redirected_to console_principal_path(principal.oid)
      assert_equal "Deleted Discord channel permission.", flash[:notice]
      assert_raises(ActiveRecord::RecordNotFound) { delete_permission.reload }
      assert_predicate keep_permission.reload, :persisted?
    end

    test "destroy deletes a role permission and redirects to the role" do
      role = roles(:acme_infra)
      delete_permission = role.discord_channel_permissions.create!(
        channel_id: "123456789012345678",
        upload_enabled: true
      )
      keep_permission = role.discord_channel_permissions.create!(
        channel_id: "987654321098765432",
        upload_enabled: true
      )

      assert_difference -> { role.discord_channel_permissions.count }, -1 do
        delete console_discord_channel_permission_url(delete_permission.oid)
      end

      assert_redirected_to console_role_path(role.oid)
      assert_equal "Deleted Discord channel permission.", flash[:notice]
      assert_raises(ActiveRecord::RecordNotFound) { delete_permission.reload }
      assert_predicate keep_permission.reload, :persisted?
    end
  end
end
