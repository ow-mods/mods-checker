#!/usr/bin/env nu

cargo build --release

let mods = owmods raw | from json | select uniqueName repo;

let data = $mods | each {|it| 
	sleep 100ms;  
	print $"vv== ($it.uniqueName) ==vv";
	let res = ./target/result/mods-checker repo ($it.repo | str substring 19..) -n $it.uniqueName -d -r | from json;
	let output = $res | insert uniqueName $it.uniqueName | reject -o url;
	print $"^^== ($output.error | default 'No Error') ==^^";
	$output
};

$data | save -f data.json;

