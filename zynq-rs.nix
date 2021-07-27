let
  pkgs = import <nixpkgs> {};
in
  pkgs.fetchgit {
    url = "https://git.m-labs.hk/M-Labs/zynq-rs.git";
    rev = "2c161720fa12f8b7abecaf60f77b062b08ac9bc1";
    sha256 = "0zpazkicqzps86r7lgqf09y9ary94mjvxw6gc41z9kjjyxar5fhr";
  }
