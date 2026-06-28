{
  lib,
  ...
}: let
  inherit (builtins) readFile fromTOML filter listToAttrs map attrNames fetchTree length;
  inherit (lib) hasPrefix removePrefix splitString last head isString;

  parseSource = source: let
    noPrefix = removePrefix "git+" source;
    urlFragments = splitString "#" noPrefix;
    urlAndQuery = head urlFragments;
    fragment = last urlFragments;
    urlParts = splitString "?" urlAndQuery;
    baseUrl = head urlParts;
    queryString =
      if length urlParts > 1
      then last urlParts
      else "";
    queryParams =
      if queryString == ""
      then {}
      else
        listToAttrs (map (param: let
            kv = splitString "=" param;
          in {
            name = head kv;
            value = last kv;
          })
        (splitString "&" queryString));
    rev =
      if queryParams ? rev
      then queryParams.rev
      else if fragment != urlAndQuery
      then fragment
      else throw "No rev or fragment found in git source: ${source}";
  in {
    url = baseUrl;
    inherit rev;
  };

  computeHash = {url, rev}: (
    fetchTree {
      type = "git";
      inherit url rev;
      allRefs = true;
    }
  ).narHash;
in {
  compute = lockFilePath: let
    lockData = fromTOML (readFile lockFilePath);
    gitPackages = filter (p: p ? source && hasPrefix "git+" p.source) lockData.package;
    deduped = listToAttrs (map (p: {name = p.source; value = p;}) gitPackages);
    sources = attrNames deduped;
  in
    listToAttrs (map (source: {
      name = source;
      value = computeHash (parseSource source);
    }) sources);
}
