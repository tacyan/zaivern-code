-- 起動中のアプリのうち、前面ウィンドウの「active tab」を持つもの (= タブ付きブラウザ) を探す。
-- 見つかったアプリ名を返す。無ければ空文字を返す。製品名は一切埋め込まない。
set foundName to ""
try
	tell application "System Events"
		set procNames to name of every application process whose background only is false
	end tell
on error
	return ""
end try

repeat with procName in procNames
	set thisName to (procName as text)
	try
		with timeout of 3 seconds
			set theUrl to (run script "tell application \"" & thisName & "\" to get URL of active tab of front window")
		end timeout
		if theUrl is not missing value then
			set foundName to thisName
			exit repeat
		end if
	end try
end repeat

return foundName
