--
-- PostgreSQL database dump
--

\restrict p5RestoreFixtureKey

-- Dumped from database version 17.0
-- Dumped by pg_dump version 17.0

SET statement_timeout = 0;
SET lock_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET client_min_messages = warning;

SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: p5_restore_copy; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.p5_restore_copy (
    id integer NOT NULL,
    name text
);

--
-- Data for Name: p5_restore_copy; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.p5_restore_copy (id, name) FROM stdin;
1	Nguyễn Văn A
2	\N
3	semi;colon
\.


--
-- PostgreSQL database dump complete
--

\unrestrict p5RestoreFixtureKey

