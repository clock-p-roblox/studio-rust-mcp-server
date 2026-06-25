local TextFormat = {}

function TextFormat.trim(value: string): string
	return value:gsub("^%s+", ""):gsub("%s+$", "")
end

function TextFormat.summarizeText(value: string?): string
	if value == nil or value == "" then
		return ""
	end

	if #value <= 180 then
		return value
	end

	return string.sub(value, 1, 180) .. "..."
end

function TextFormat.formatAgeMs(value: number?): string?
	if value == nil then
		return nil
	end

	if value < 1000 then
		return "just now"
	end

	local seconds = math.floor(value / 1000)
	if seconds < 60 then
		return tostring(seconds) .. "s ago"
	end

	local minutes = math.floor(seconds / 60)
	if minutes < 60 then
		return tostring(minutes) .. "m ago"
	end

	local hours = math.floor(minutes / 60)
	return tostring(hours) .. "h ago"
end

function TextFormat.joinSentences(parts: { string }): string
	local filtered = {}
	for _, part in parts do
		if part ~= "" then
			table.insert(filtered, part)
		end
	end

	return table.concat(filtered, " ")
end

return TextFormat
