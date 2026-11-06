module Console
  module DiscordChannelPermissionManagement
    extend ActiveSupport::Concern

    private

    def load_discord_channel_permission_form(owner)
      @discord_channel_permissions = owner.discord_channel_permissions.ordered
    end

    def update_discord_channel_permissions_from_form(owner, path)
      rows = discord_channel_permission_form_rows(owner)
      if discord_channel_permission_rows_unchanged?(owner, rows)
        return redirect_to path, notice: "Updated Discord channel permissions."
      end

      DiscordChannelPermission.replace_for!(owner, rows)
      redirect_to path, notice: "Updated Discord channel permissions."
    rescue ActiveRecord::RecordInvalid => e
      redirect_to path, alert: e.record.errors.full_messages.to_sentence
    rescue ActiveRecord::ReadonlyAttributeError
      redirect_to path, alert: "Discord channels cannot be changed after creation."
    rescue ActiveRecord::RecordNotUnique
      redirect_to path, alert: "Each Discord channel can only be selected once."
    end

    def discord_channel_permission_rows_unchanged?(owner, rows)
      owner.discord_channel_permissions_payload == DiscordChannelPermission.permission_rows_payload(rows)
    end

    def discord_channel_permission_form_rows(owner)
      permitted = params.require(owner.model_name.param_key).permit(
        discord_channel_permissions_attributes: [
          :id,
          :channel_id,
          :_destroy,
          *DiscordChannelPermission::PERMISSION_ATTRIBUTES
        ]
      )
      rows = permitted.fetch(:discord_channel_permissions_attributes, {}).values
      existing = owner.discord_channel_permissions.index_by { |permission| permission.id.to_s }

      rows.filter_map do |row|
        attrs = row.to_h.with_indifferent_access
        next if ActiveModel::Type::Boolean.new.cast(attrs[:_destroy])

        channel_id = discord_channel_id_for_form_row(attrs, existing)
        next if channel_id.blank?

        { channel_id: channel_id }.merge(
          DiscordChannelPermission::PERMISSION_ATTRIBUTES.to_h do |permission|
            [ permission, ActiveModel::Type::Boolean.new.cast(attrs[permission]) ]
          end
        )
      end
    end

    def discord_channel_id_for_form_row(attrs, existing)
      return attrs[:channel_id] if attrs[:id].blank?

      permission = existing.fetch(attrs[:id].to_s) { raise ActiveRecord::RecordNotFound }
      submitted = attrs[:channel_id].to_s.strip
      raise ActiveRecord::ReadonlyAttributeError if submitted.present? && submitted != permission.channel_id

      permission.channel_id
    end
  end
end
