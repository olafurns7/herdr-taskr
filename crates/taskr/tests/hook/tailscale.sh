#!/bin/sh
case "$1" in
ip) printf '::1
' ;;
status)
 name=host-a
 [ "$NET_ID" = hub ] && name=hub
 printf '{"MagicDNSSuffix":"example.ts.net","Self":{"ID":"%s-node","DNSName":"%s.example.ts.net.","UserID":1,"TailscaleIPs":[]},"User":{"1":{"LoginName":"owner@example.com"}}}
' "$name" "$name" ;;
whois)
 name=host-a
 case "$3" in hub-*) name=hub ;; esac
 [ -f "$HOME/whois-host" ] && name=$(cat "$HOME/whois-host")
 login=owner@example.com; tags='[]'
 case "$3:$NET_DENY_HUB" in hub-*:user) login=other@example.com ;; hub-*:tag) tags='["tag:ci"]' ;; hub-*:bad) tags='{}' ;; esac
 printf '{"Node":{"StableID":"%s-node","Name":"%s.example.ts.net.","Tags":%s},"UserProfile":{"LoginName":"%s"}}
' "$name" "$name" "$tags" "$login" ;;
*) exit 2 ;;
esac
