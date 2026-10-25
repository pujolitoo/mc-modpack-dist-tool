# Third-party notices

## authlib-injector
When `[auth] url` is set in `launcher.toml`, the launcher embeds an **unaltered** copy of authlib-injector
(`assets/authlib-injector.jar`) and loads it as a Java agent into the game.

- Project and source code: https://github.com/yushijinhun/authlib-injector
- License: GNU Affero General Public License v3.0 or later, with the "AUTHLIB-INJECTOR" exception:

      As a special exception, using this work in the following ways does not cause your program to be covered by
      the AGPL:
        a) Bundling the unaltered binary form of this work in your program without statically or dynamically
           linking to it; or
        b) Interacting with this work through the provided inter-process communication interface, such as the
           HTTP API; or
        c) Loading this work as a Java Agent into a Java Virtual Machine.

  The full license text is included inside the jar (`META-INF/licenses/authlib-injector.txt`).

## zlib
Linux builds of the launcher embed an unmodified copy of the classic zlib shared library (`libz.so.1`), taken from the
build machine, to work around distributions that replace zlib with zlib-ng (see `src/compat.rs`).

zlib is distributed under the zlib license:

    Copyright (C) 1995-2024 Jean-loup Gailly and Mark Adler

    This software is provided 'as-is', without any express or implied warranty. In no event will the authors be held
    liable for any damages arising from the use of this software.

    Permission is granted to anyone to use this software for any purpose, including commercial applications, and to
    alter it and redistribute it freely, subject to the following restrictions:

    1. The origin of this software must not be misrepresented; you must not claim that you wrote the original software.
       If you use this software in a product, an acknowledgment in the product documentation would be appreciated but
       is not required.
    2. Altered source versions must be plainly marked as such, and must not be misrepresented as being the original
       software.
    3. This notice may not be removed or altered from any source distribution.

    Jean-loup Gailly (jloup@gzip.org) / Mark Adler (madler@alumni.caltech.edu)
