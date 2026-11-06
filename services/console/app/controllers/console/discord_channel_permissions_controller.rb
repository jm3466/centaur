module Console
  class DiscordChannelPermissionsController < ApplicationController
    layout "console"

    before_action :require_admin

    def destroy
      permission = DiscordChannelPermission.find_by_oid!(params.require(:discord_channel_permission_id))
      redirect_path = owner_path(permission)

      permission.destroy!
      redirect_to redirect_path, notice: "Deleted Discord channel permission."
    end

    private

    def owner_path(permission)
      if permission.principal.present?
        console_principal_path(permission.principal.oid)
      else
        console_role_path(permission.role.oid)
      end
    end
  end
end
