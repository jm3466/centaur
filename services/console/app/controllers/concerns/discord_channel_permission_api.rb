module DiscordChannelPermissionApi
  extend ActiveSupport::Concern

  InvalidDiscordChannelPermissions = Class.new(StandardError)

  included do
    rescue_from InvalidDiscordChannelPermissions, with: :render_discord_channel_permissions_error
  end

  def upsert_discord_channel_permission
    owner = discord_channel_permission_owner
    attrs = upsert_discord_channel_permission_params
    attrs[:channel_id] = attrs[:channel_id].to_s.strip
    permission, was_new = save_discord_channel_permission_with_race_retry!(owner, attrs)

    render status: (was_new ? :created : :ok), json: { data: permission.as_permission_json }
  rescue ActiveRecord::RecordInvalid => e
    render_validation_error(e.record)
  end

  private

  def save_discord_channel_permission_with_race_retry!(owner, attrs)
    save_discord_channel_permission!(owner, attrs)
  rescue ActiveRecord::RecordNotUnique
    permission = owner.discord_channel_permissions.find_by!(channel_id: attrs[:channel_id])
    permission.assign_attributes(attrs.except(:channel_id))
    permission.save!
    [ permission, false ]
  end

  def save_discord_channel_permission!(owner, attrs)
    permission = owner.discord_channel_permissions.find_or_initialize_by(channel_id: attrs[:channel_id])
    was_new = permission.new_record?
    permission.assign_attributes(DiscordChannelPermission::DEFAULT_ENABLED_ATTRIBUTES) if was_new
    permission.assign_attributes(was_new ? attrs : attrs.except(:channel_id))
    permission.save!
    [ permission, was_new ]
  end

  def replace_discord_channel_permissions!(owner)
    DiscordChannelPermission.replace_for!(owner, discord_channel_permission_params)
  end

  def discord_channel_permission_params
    raw = data_params[:discord_channel_permissions]
    unless raw.nil? || raw.is_a?(Array)
      raise InvalidDiscordChannelPermissions, "discord_channel_permissions must be an array"
    end

    rows = data_params.permit(
      discord_channel_permissions: [
        :channel_id,
        *DiscordChannelPermission::PERMISSION_ATTRIBUTES
      ]
    ).fetch(:discord_channel_permissions, [])

    if raw.present? && rows.length != raw.length
      raise InvalidDiscordChannelPermissions, "discord_channel_permissions rows must be objects"
    end

    rows
  end

  def upsert_discord_channel_permission_params
    data_params.permit(
      :channel_id,
      *DiscordChannelPermission::PERMISSION_ATTRIBUTES
    )
  end

  def render_discord_channel_permissions_error(error)
    render_error(status: :unprocessable_entity, message: error.message)
  end
end
