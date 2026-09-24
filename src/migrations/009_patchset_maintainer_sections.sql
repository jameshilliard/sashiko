-- Copyright 2026 The Sashiko Authors
--
-- Licensed under the Apache License, Version 2.0 (the "License");
-- you may not use this file except in compliance with the License.
-- You may obtain a copy of the License at
--
--     https://www.apache.org/licenses/LICENSE-2.0
--
-- Unless required by applicable law or agreed to in writing, software
-- distributed under the License is distributed on an "AS IS" BASIS,
-- WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
-- See the License for the specific language governing permissions and
-- limitations under the License.

-- The MAINTAINERS sections a patchset touches, and who may therefore read its
-- raw review transcripts.
--
-- This is not patchsets_subsystems. That table holds mailing list labels taken
-- from To:/Cc: headers and configured path regexes; it names lists rather than
-- people, anybody can put an address in Cc:, and it confers nothing. The rows
-- here are section titles matched out of the kernel MAINTAINERS file against
-- the files the series actually changes, which is the only attribution that
-- names people the kernel trusts with that code. The table is named for what
-- it holds so the two cannot be confused in a query.
--
-- The schema mirrors bug_subsystems deliberately, down to the source column
-- and its default, so that one SubsystemSource type serves both and a reader
-- who understands bug attribution already understands this. Only
-- 'maintainers_section' confers authority; the default is the least privileged
-- value so that a writer which forgets to say where a name came from grants
-- nobody anything.
CREATE TABLE IF NOT EXISTS patchset_maintainer_sections (
    patchset_id INTEGER NOT NULL,
    subsystem TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'caller_supplied'
        CHECK (source IN ('maintainers_section', 'path_prefix', 'caller_supplied')),
    PRIMARY KEY (patchset_id, subsystem),
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_patchset_maintainer_sections_subsystem
    ON patchset_maintainer_sections(subsystem, patchset_id);

-- Serves the authorization lookup, which only ever asks for the rows that can
-- confer authority.
CREATE INDEX IF NOT EXISTS idx_patchset_maintainer_sections_authorizing
    ON patchset_maintainer_sections(patchset_id, subsystem)
    WHERE source = 'maintainers_section';
