CREATE TABLE "evil
::error file=README.md,line=1::INJECTED-TABLE" (t text NOT NULL, "c
::stop-commands::tok1
" boolean NOT NULL, x bigint NOT NULL);
CREATE TABLE "evil::warning::INJECTED-CR" (t text NOT NULL, "c
::stop-commands::tok1
" boolean NOT NULL, x bigint NOT NULL);
CREATE TABLE "evil%0A::error::INJECTED-PCT" (t text NOT NULL, "c
::stop-commands::tok1
" boolean NOT NULL, x bigint NOT NULL);
CREATE TABLE "evil
::add-mask::secret" (t text NOT NULL, "c
::stop-commands::tok1
" boolean NOT NULL, x bigint NOT NULL);
CREATE TABLE ttv ("a
::error::FRONTIER-COL" timetz NOT NULL, b timetz NOT NULL, "v
::set-output name=x::y" text NOT NULL);
CREATE TABLE notes1 (a "evil
::error::TYPE-NOTE" NOT NULL, b bigint NOT NULL);
ALTER TABLE "ghost
::error::ALTER-NOTE" ADD COLUMN z int;
CREATE TABLE dropme (a int, "b
::error::DROP-NOTE" text); ALTER TABLE dropme DROP COLUMN "b
::error::DROP-NOTE";
