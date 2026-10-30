<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.bluewhale.lanlock</string>
	<key>ProgramArguments</key>
	<array>
		<string>__PREFIX__/lanlock.sh</string>
		<string>apply</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>WatchPaths</key>
	<array>
		<string>__PREFIX__/allow.txt</string>
	</array>
	<key>StandardOutPath</key>
	<string>/var/log/lanlock.out</string>
	<key>StandardErrorPath</key>
	<string>/var/log/lanlock.err</string>
</dict>
</plist>
